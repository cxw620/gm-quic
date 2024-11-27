use std::sync::Arc;

use bytes::BufMut;
use futures::{channel::mpsc, StreamExt};
use qbase::{
    cid::ConnectionId,
    frame::{io::WriteFrame, AckFrame, ConnectionCloseFrame, Frame, FrameReader, ReceiveFrame},
    packet::{
        decrypt::{decrypt_packet, remove_protection_of_long_packet},
        encrypt::{encode_long_first_byte, encrypt_packet, protect_header},
        header::{io::WriteHeader, long::io::LongHeaderBuilder, EncodeHeader, GetType},
        keys::ArcKeys,
        number::WritePacketNumber,
        AssembledPacket, DataPacket, PacketNumber, PacketWriter,
    },
    varint::{EncodeBytes, VarInt, WriteVarInt},
    Epoch,
};
use qcongestion::{CongestionControl, TrackPackets, MSS};
use qrecovery::{
    crypto::{CryptoStream, CryptoStreamOutgoing},
    journal::{ArcRcvdJournal, HandshakeJournal},
};
use tokio::{sync::Notify, task::JoinHandle};

use super::any;
use crate::{
    conn::{transmit::HandshakeSpaceReader, RcvdPackets},
    error::ConnError,
    path::{ArcPaths, Path},
    pipe,
    tx::{PacketMemory, Transaction},
};

#[derive(Clone)]
pub struct HandshakeSpace {
    pub keys: ArcKeys,
    pub journal: HandshakeJournal,
    pub crypto_stream: CryptoStream,
}

impl Default for HandshakeSpace {
    fn default() -> Self {
        Self {
            keys: ArcKeys::new_pending(),
            journal: HandshakeJournal::with_capacity(16),
            crypto_stream: CryptoStream::new(4096, 4096),
        }
    }
}

impl HandshakeSpace {
    pub fn build(
        &self,
        rcvd_packets: RcvdPackets,
        pathes: &ArcPaths,
        notify: &Arc<Notify>,
        conn_error: &ConnError,
    ) -> JoinHandle<RcvdPackets> {
        let (crypto_frames_entry, rcvd_crypto_frames) = mpsc::unbounded();
        let (ack_frames_entry, rcvd_ack_frames) = mpsc::unbounded();

        let dispatch_frame = {
            let conn_error = conn_error.clone();
            move |frame: Frame, path: &Path| match frame {
                Frame::Ack(f) => {
                    path.cc().on_ack(Epoch::Initial, &f);
                    _ = ack_frames_entry.unbounded_send(f);
                }
                Frame::Close(f) => conn_error.on_ccf_rcvd(&f),
                Frame::Crypto(f, bytes) => _ = crypto_frames_entry.unbounded_send((f, bytes)),
                Frame::Padding(_) | Frame::Ping(_) => {}
                _ => unreachable!("unexpected frame: {:?} in handshake packet", frame),
            }
        };
        let on_data_acked = {
            let crypto_stream_outgoing = self.crypto_stream.outgoing();
            let sent_journal = self.journal.of_sent_packets();
            move |ack_frame: &AckFrame| {
                let mut ack_guard = sent_journal.for_ack();
                ack_guard.update_largest(ack_frame.largest.into_inner());

                for pn in ack_frame.iter().flat_map(|r| r.rev()) {
                    for frame in ack_guard.on_pkt_acked(pn) {
                        crypto_stream_outgoing.on_data_acked(&frame);
                    }
                }
            }
        };

        pipe!(@error(conn_error) rcvd_crypto_frames |> self.crypto_stream.incoming(), recv_frame);
        pipe!(rcvd_ack_frames |> on_data_acked);
        self.parse_rcvd_packets_and_dispatch_frames(
            rcvd_packets,
            pathes,
            dispatch_frame,
            notify,
            conn_error,
        )
    }

    fn parse_rcvd_packets_and_dispatch_frames(
        &self,
        mut rcvd_packets: RcvdPackets,
        pathes: &ArcPaths,
        dispatch_frame: impl Fn(Frame, &Path) + Send + 'static,
        notify: &Arc<Notify>,
        conn_error: &ConnError,
    ) -> JoinHandle<RcvdPackets> {
        let pathes = pathes.clone();
        let conn_error = conn_error.clone();
        let notify = notify.clone();
        tokio::spawn({
            let rcvd_journal = self.journal.of_rcvd_packets();
            let keys = self.keys.clone();
            async move {
                while let Some((mut packet, pathway, usc)) = any(rcvd_packets.next(), &notify).await
                {
                    let pty = packet.header.get_type();
                    let Some(keys) = any(keys.get_remote_keys(), &notify).await else {
                        break;
                    };
                    let undecoded_pn = match remove_protection_of_long_packet(
                        keys.remote.header.as_ref(),
                        packet.bytes.as_mut(),
                        packet.offset,
                    ) {
                        Ok(Some(pn)) => pn,
                        Ok(None) => continue,
                        Err(invalid_reserved_bits) => {
                            conn_error.on_error(invalid_reserved_bits.into());
                            break;
                        }
                    };

                    let pn = match rcvd_journal.decode_pn(undecoded_pn) {
                        Ok(pn) => pn,
                        // TooOld/TooLarge/HasRcvd
                        Err(_e) => continue,
                    };
                    let body_offset = packet.offset + undecoded_pn.size();
                    let decrypted = decrypt_packet(
                        keys.remote.packet.as_ref(),
                        pn,
                        packet.bytes.as_mut(),
                        body_offset,
                    );
                    let Ok(pkt_len) = decrypted else { continue };

                    let path = pathes.get_or_create(pathway, usc);
                    path.on_rcvd(packet.bytes.len());

                    let _header = packet.bytes.split_to(body_offset);
                    packet.bytes.truncate(pkt_len);

                    // See [RFC 9000 section 8.1](https://www.rfc-editor.org/rfc/rfc9000.html#name-address-validation-during-c)
                    // Once an endpoint has successfully processed a Handshake packet from the peer, it can consider the peer
                    // address to have been validated.
                    // It may have already been verified using tokens in the Initial space
                    path.grant_anti_amplifier();

                    match FrameReader::new(packet.bytes.freeze(), pty).try_fold(
                        false,
                        |is_ack_packet, frame| {
                            let (frame, is_ack_eliciting) = frame?;
                            dispatch_frame(frame, &path);
                            Ok(is_ack_packet || is_ack_eliciting)
                        },
                    ) {
                        Ok(is_ack_packet) => {
                            rcvd_journal.register_pn(pn);
                            path.cc().on_pkt_rcvd(Epoch::Handshake, pn, is_ack_packet);
                        }
                        Err(e) => conn_error.on_error(e),
                    }
                }
                rcvd_packets
            }
        })
    }

    pub fn try_assemble<'b>(
        &self,
        tx: &mut Transaction<'_>,
        buf: &'b mut [u8],
    ) -> Option<(AssembledPacket<'b>, Option<u64>)> {
        let keys = self.keys.get_local_keys()?;
        let sent_journal = self.journal.of_sent_packets();
        let mut packet = PacketMemory::new(
            LongHeaderBuilder::with_cid(tx.dcid(), tx.scid()).handshake(),
            buf,
            keys.local.packet.tag_len(),
            &sent_journal,
        )?;

        let mut ack = None;
        if let Some((largest, rcvd_time)) = tx.need_ack(Epoch::Handshake) {
            let rcvd_journal = self.journal.of_rcvd_packets();
            if let Some(ack_frame) =
                rcvd_journal.gen_ack_frame_util(largest, rcvd_time, packet.remaining_mut())
            {
                packet.dump_ack_frame(ack_frame);
                ack = Some(largest);
            }
        }

        // TODO: 可以封装在CryptoStream中，当成一个函数
        //      crypto_stream.try_load_data_into(&mut packet);
        let crypto_stream_outgoing = self.crypto_stream.outgoing();
        crypto_stream_outgoing.try_load_data_into(&mut packet);

        let packet: PacketWriter<'b> = packet.try_into().ok()?;
        Some((
            packet.encrypt_long_packet(keys.local.header.as_ref(), keys.local.packet.as_ref()),
            ack,
        ))
    }

    pub fn reader(&self) -> HandshakeSpaceReader {
        HandshakeSpaceReader {
            keys: self.keys.clone(),
            journal: self.journal.clone(),
            crypto_stream_outgoing: self.crypto_stream.outgoing(),
        }
    }
}

#[derive(Clone)]
pub struct ClosingHandshakeScope {
    keys: Arc<rustls::quic::Keys>,
    rcvd_journal: ArcRcvdJournal,
    // 发包时用得着
    next_sending_pn: (u64, PacketNumber),
}

impl ClosingHandshakeScope {
    pub fn assemble_ccf_packet(
        &self,
        buf: &mut [u8; MSS],
        ccf: &ConnectionCloseFrame,
        scid: ConnectionId,
        dcid: ConnectionId,
    ) -> usize {
        let (pk, hk) = (
            self.keys.local.packet.as_ref(),
            self.keys.local.header.as_ref(),
        );

        let hdr = LongHeaderBuilder::with_cid(dcid, scid).handshake();
        let (mut hdr_buf, payload_tag) = buf.split_at_mut(hdr.size() + 2);
        let payload_tag_len = payload_tag.len();
        let tag_len = pk.tag_len();
        let payload_buf = &mut payload_tag[..payload_tag_len - tag_len];

        let (pn, encoded_pn) = self.next_sending_pn;
        let (mut pn_buf, mut body_buf) = payload_buf.split_at_mut(encoded_pn.size());

        let body_size = body_buf.remaining_mut();
        body_buf.put_frame(ccf);
        let mut body_len = body_size - body_buf.remaining_mut();

        let hdr_len = hdr_buf.len();
        let pn_len = pn_buf.len();
        if pn_len + body_len + tag_len < 20 {
            let padding_len = 20 - pn_len - body_len - tag_len;
            body_buf.put_bytes(0, padding_len);
            body_len += padding_len;
        }
        let pkt_size = hdr_len + pn_len + body_len + tag_len;

        hdr_buf.put_header(&hdr);
        hdr_buf.encode_varint(
            &VarInt::try_from(pn_len + body_len + tag_len).unwrap(),
            EncodeBytes::Two,
        );
        pn_buf.put_packet_number(encoded_pn);

        encode_long_first_byte(&mut buf[0], pn_len);
        encrypt_packet(pk, pn, &mut buf[..pkt_size], hdr_len + pn_len);
        protect_header(hk, &mut buf[..pkt_size], hdr_len, pn_len);

        pkt_size
    }
}

impl TryFrom<HandshakeSpace> for ClosingHandshakeScope {
    type Error = ();

    fn try_from(hs: HandshakeSpace) -> Result<Self, Self::Error> {
        let Some(keys) = hs.keys.invalid() else {
            return Err(());
        };
        let rcvd_journal = hs.journal.of_rcvd_packets();
        let next_sending_pn = hs.journal.of_sent_packets().new_packet().pn();

        Ok(Self {
            keys,
            rcvd_journal,
            next_sending_pn,
        })
    }
}

impl super::RecvPacket for ClosingHandshakeScope {
    fn has_rcvd_ccf(&self, mut packet: DataPacket) -> bool {
        let undecoded_pn = match remove_protection_of_long_packet(
            self.keys.remote.header.as_ref(),
            packet.bytes.as_mut(),
            packet.offset,
        ) {
            Ok(Some(pn)) => pn,
            _ => return false,
        };

        let pn = match self.rcvd_journal.decode_pn(undecoded_pn) {
            Ok(pn) => pn,
            // TooOld/TooLarge/HasRcvd
            Err(_e) => return false,
        };
        let body_offset = packet.offset + undecoded_pn.size();
        Self::decrypt_and_parse(self.keys.remote.packet.as_ref(), pn, packet, body_offset)
    }
}

#[derive(Clone)]
pub struct HandshakeTracker {
    journal: HandshakeJournal,
    outgoing: CryptoStreamOutgoing,
}

impl HandshakeTracker {
    pub fn new(journal: HandshakeJournal, outgoing: CryptoStreamOutgoing) -> Self {
        Self { journal, outgoing }
    }
}

impl TrackPackets for HandshakeTracker {
    fn may_loss(&self, pn: u64) {
        for frame in self.journal.of_sent_packets().for_ack().may_loss_pkt(pn) {
            self.outgoing.may_loss_data(&frame);
        }
    }

    fn retire(&self, pn: u64) {
        self.journal.of_rcvd_packets().write().retire(pn);
    }
}
