use std::{
    collections::VecDeque,
    time::{Duration, Instant},
};

use bytes::{Bytes, BytesMut};
use rustc_hash::FxHashSet;
use smol::channel::Receiver;

use crate::{
    mux::{
        congestion::{CongestionControl, Cubic, Reno},
        structs::*,
    },
    safe_deserialize, MyFutureExt,
};

use super::{
    bipe::{BipeReader, BipeWriter},
    inflight::Inflight,
    MSS,
};
use smol::prelude::*;

pub(crate) struct ConnVars {
    pub pre_inflight: VecDeque<Message>,
    pub inflight: Inflight,
    pub next_free_seqno: Seqno,
    pub retrans_count: u64,

    pub delayed_ack_timer: Option<Instant>,
    pub ack_seqnos: FxHashSet<Seqno>,

    pub reorderer: Reorderer<Bytes>,
    pub lowest_unseen: Seqno,

    closing: bool,
    write_fragments: VecDeque<Bytes>,
    next_pace_time: Instant,
    lost_seqnos: Vec<Seqno>,
    last_loss: Option<Instant>,

    cc: Box<dyn CongestionControl + Send>,
}

impl Default for ConnVars {
    fn default() -> Self {
        ConnVars {
            pre_inflight: VecDeque::new(),
            inflight: Inflight::new(),
            next_free_seqno: 0,
            retrans_count: 0,

            delayed_ack_timer: None,
            ack_seqnos: FxHashSet::default(),

            reorderer: Reorderer::default(),
            lowest_unseen: 0,

            closing: false,

            write_fragments: VecDeque::new(),

            next_pace_time: Instant::now(),

            lost_seqnos: Vec::new(),
            last_loss: None,
            cc: Box::new(Cubic::new(0.7, 0.4)),
        }
    }
}

const ACK_BATCH: usize = 16;

#[derive(Debug)]
enum ConnVarEvt {
    Rto(Seqno),
    Retransmit(Seqno),
    AckTimer,
    NewWrite(Bytes),
    NewPkt(Message),
    Closing,
}

impl ConnVars {
    /// Process a *single* event. Errors out when the thing should be closed.
    pub async fn process_one(
        &mut self,
        stream_id: u16,
        recv_write: &mut BipeReader,
        send_read: &mut BipeWriter,
        recv_wire_read: &Receiver<Message>,
        transmit: impl Fn(Message),
    ) -> anyhow::Result<()> {
        match self.next_event(recv_write, recv_wire_read).await {
            Ok(ConnVarEvt::Retransmit(seqno)) => {
                self.lost_seqnos.retain(|v| *v != seqno);
                if let Some(msg) = self.inflight.retransmit(seqno) {
                    tracing::trace!(
                        "** RETRANSMIT {} (inflight = {}, cwnd = {}, lost_count = {}) **",
                        seqno,
                        self.inflight.inflight(),
                        self.cc.cwnd(),
                        self.inflight.lost_count(),
                    );
                    transmit(msg);
                }
                Ok(())
            }
            Ok(ConnVarEvt::Closing) => {
                self.closing = true;
                self.check_closed()?;
                Ok(())
            }
            Ok(ConnVarEvt::Rto(seqno)) => {
                tracing::trace!(
                    "** MARKING LOST {} (unacked = {}, inflight = {}, cwnd = {}, lost_count = {}) **",
                    seqno,
                    self.inflight.unacked(),
                    self.inflight.inflight(),
                    self.cc.cwnd(),
                    self.inflight.lost_count(),
                );
                let now = Instant::now();
                if let Some(old) = self.last_loss.replace(now) {
                    if now.saturating_duration_since(old) > self.inflight.rto() {
                        self.cc.mark_loss()
                    }
                } else {
                    self.cc.mark_loss();
                }
                self.inflight.mark_lost(seqno);
                self.lost_seqnos.push(seqno);
                Ok(())
            }
            Ok(ConnVarEvt::NewPkt(Message::Rel {
                kind: RelKind::Rst, ..
            })) => anyhow::bail!("received RST"),
            Ok(ConnVarEvt::NewPkt(Message::Rel {
                kind: RelKind::DataAck,
                payload,
                seqno,
                ..
            })) => {
                let seqnos = safe_deserialize::<Vec<Seqno>>(&payload)?;
                tracing::trace!("new ACK pkt with {} seqnos", seqnos.len());
                for seqno in seqnos {
                    self.lost_seqnos.retain(|v| *v != seqno);
                    if self.inflight.mark_acked(seqno) {
                        self.cc.mark_ack();
                    }
                }
                self.inflight.mark_acked_lt(seqno);
                self.check_closed()?;
                Ok(())
            }
            Ok(ConnVarEvt::NewPkt(Message::Rel {
                kind: RelKind::Data,
                seqno,
                payload,
                ..
            })) => {
                tracing::trace!("new data pkt with seqno={}", seqno);
                if self.delayed_ack_timer.is_none() {
                    self.delayed_ack_timer = Instant::now().checked_add(Duration::from_millis(1));
                }
                if self.reorderer.insert(seqno, payload) {
                    self.ack_seqnos.insert(seqno);
                }
                let times = self.reorderer.take();
                self.lowest_unseen += times.len() as u64;
                let mut success = true;
                for pkt in times {
                    success |= send_read.write(&pkt).await.is_ok();
                }
                if success {
                    Ok(())
                } else {
                    anyhow::bail!("cannot write into send_read")
                }
            }
            Ok(ConnVarEvt::NewWrite(bts)) => {
                assert!(bts.len() <= MSS);
                // self.limiter.wait(implied_rate).await;
                let seqno = self.next_free_seqno;
                self.next_free_seqno += 1;
                let msg = Message::Rel {
                    kind: RelKind::Data,
                    stream_id,
                    seqno,
                    payload: bts,
                };
                // put msg into inflight
                self.inflight.insert(seqno, msg.clone());

                transmit(msg);

                Ok(())
            }
            Ok(ConnVarEvt::AckTimer) => {
                // eprintln!("acking {} seqnos", conn_vars.ack_seqnos.len());
                let mut ack_seqnos: Vec<_> = self.ack_seqnos.iter().collect();
                assert!(ack_seqnos.len() <= ACK_BATCH);
                ack_seqnos.sort_unstable();
                let encoded_acks = bincode::serialize(&ack_seqnos).unwrap();
                if encoded_acks.len() > 1000 {
                    tracing::warn!("encoded_acks {} bytes", encoded_acks.len());
                }
                transmit(Message::Rel {
                    kind: RelKind::DataAck,
                    stream_id,
                    seqno: self.lowest_unseen,
                    payload: Bytes::copy_from_slice(&encoded_acks),
                });
                self.ack_seqnos.clear();

                self.delayed_ack_timer = None;

                Ok(())
            }
            Err(err) => {
                tracing::debug!("forced to RESET due to {:?}", err);
                anyhow::bail!(err);
            }
            evt => {
                tracing::debug!("unrecognized event: {:#?}", evt);
                Ok(())
            }
        }
    }

    /// Checks the closed flag.
    fn check_closed(&self) -> anyhow::Result<()> {
        if self.closing && self.inflight.unacked() == 0 {
            anyhow::bail!("closing flag set and unacked == 0, so dying");
        }
        Ok(())
    }
    /// Gets the next event.
    async fn next_event(
        &mut self,
        recv_write: &mut BipeReader,
        recv_wire_read: &Receiver<Message>,
    ) -> anyhow::Result<ConnVarEvt> {
        // smol::future::yield_now().await;
        // There's a rather subtle logic involved here.
        //
        // We want to make sure the *total inflight* is less than cwnd.
        // This is very tricky when a packet is lost and must be transmitted.
        // We don't want retransmissions to cause more than CWND packets in flight, any more do we let normal transmissions do so.
        // Thus, we must have a state where a packet is known to be lost, but is not yet retransmitted.
        let first_retrans = self.lost_seqnos.get(0).cloned();
        let can_retransmit = self.inflight.inflight() <= self.cc.cwnd();
        // If we've already closed the connection, we cannot write *new* packets
        let can_write_new =
            can_retransmit && self.inflight.unacked() <= self.cc.cwnd() && !self.closing;
        let force_ack = self.ack_seqnos.len() >= ACK_BATCH;
        assert!(self.ack_seqnos.len() <= ACK_BATCH);

        let ack_timer = self.delayed_ack_timer;
        let ack_timer = async {
            if force_ack {
                return Ok(ConnVarEvt::AckTimer);
            }
            if let Some(time) = ack_timer {
                smol::Timer::at(time).await;
                Ok::<ConnVarEvt, anyhow::Error>(ConnVarEvt::AckTimer)
            } else {
                smol::future::pending().await
            }
        };

        let first_rto = self.inflight.first_rto();
        let rto_timeout = async move {
            let (rto_seqno, rto_time) = first_rto.unwrap();
            smol::Timer::at(rto_time).await;
            Ok::<ConnVarEvt, anyhow::Error>(ConnVarEvt::Rto(rto_seqno))
        }
        .pending_unless(first_rto.is_some());

        let new_write = async {
            smol::Timer::at(self.next_pace_time).await;
            while self.write_fragments.is_empty() {
                let to_write = {
                    let mut bts = BytesMut::with_capacity(MSS);
                    bts.extend_from_slice(&[0; MSS]);
                    let n = recv_write.read(&mut bts).await;
                    if let Ok(n) = n {
                        let bts = bts.freeze();
                        Some(bts.slice(0..n))
                    } else {
                        None
                    }
                };
                if let Some(to_write) = to_write {
                    self.write_fragments.push_back(to_write);
                } else {
                    return Ok(ConnVarEvt::Closing);
                }
            }
            let pacing_interval = Duration::from_secs_f64(1.0 / self.pacing_rate());
            self.next_pace_time = Instant::now().max(self.next_pace_time + pacing_interval);
            Ok::<ConnVarEvt, anyhow::Error>(ConnVarEvt::NewWrite(
                self.write_fragments.pop_front().unwrap(),
            ))
        }
        .pending_unless(can_write_new);
        let new_pkt = async {
            Ok::<ConnVarEvt, anyhow::Error>(ConnVarEvt::NewPkt(recv_wire_read.recv().await?))
        };
        let final_timeout = async {
            smol::Timer::after(Duration::from_secs(600)).await;
            anyhow::bail!("final timeout within relconn actor")
        };
        let retransmit = async { Ok(ConnVarEvt::Retransmit(first_retrans.unwrap())) }
            .pending_unless(first_retrans.is_some());
        retransmit
            .or(ack_timer.or(new_pkt.or(new_write.or(rto_timeout.or(final_timeout)))))
            .await
    }

    fn pacing_rate(&self) -> f64 {
        // calculate implicit rate
        (self.cc.cwnd() as f64 / self.inflight.min_rtt().as_secs_f64()).max(200.0)
    }
}
