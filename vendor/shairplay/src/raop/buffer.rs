//! AP1 RTP packet buffer with AES-CBC decryption and ALAC decode to f32.
//!
//! Incoming RTP packets are queued by sequence number into a fixed-size circular
//! buffer. Each packet is decrypted (AES-128-CBC) and decoded (ALAC) on arrival.
//! The consumer dequeues packets in order, with silence substitution for missing
//! packets and optional retransmit requests for gaps.

use crate::codec::alac::{AlacConfig, AlacDecoder};
use aes::cipher::{BlockModeDecrypt, KeyIvInit};

/// AES-128 key length in bytes.
pub const RAOP_AESKEY_LEN: usize = 16;
/// AES-128 IV length in bytes.
pub const RAOP_AESIV_LEN: usize = 16;
/// Maximum RTP packet size (including 12-byte header).
pub(crate) const RAOP_PACKET_LEN: usize = 32768;
/// Number of slots in the circular buffer. Must be a power of two for modulo
/// indexing.
///
/// Classic AirPlay normally sends audio roughly two seconds ahead of its
/// presentation time. The old 32-frame window held only about 250 ms of ALAC
/// at 44.1 kHz, so waiting for the sender's sync timestamp caused the window
/// to flush before a frame became due. 512 frames cover more than four
/// seconds while keeping the allocation bounded.
const RAOP_BUFFER_LENGTH: usize = 512;
/// Bound preallocated decoded audio across the classic look-ahead window.
const MAX_DECODED_BUFFER_BYTES: usize = 32 * 1024 * 1024;

type Aes128CbcDec = cbc::Decryptor<aes::Aes128>;

/// A single slot in the circular buffer holding one decoded audio frame.
struct BufferEntry {
    /// Whether this slot contains a valid decoded frame.
    available: bool,
    /// RTP flags byte (first byte of RTP header).
    flags: u8,
    /// RTP payload type byte (second byte of RTP header).
    entry_type: u8,
    /// RTP sequence number.
    seqnum: u16,
    /// RTP timestamp (sample clock).
    timestamp: u32,
    /// RTP synchronization source identifier.
    ssrc: u32,
    /// Decoded F32 audio samples. Pre-allocated to max frame size.
    audio_buffer: Vec<f32>,
    /// Actual number of valid samples in `audio_buffer`.
    audio_buffer_len: usize,
}

/// Compare two RTP sequence numbers with wrapping (handles 16-bit overflow).
/// Returns negative if s1 is before s2, positive if after, zero if equal.
fn seqnum_cmp(s1: u16, s2: u16) -> i16 {
    s1.wrapping_sub(s2) as i16
}

/// Parse the SDP `fmtp` attribute into an ALAC configuration.
/// Format: `96 <frame_length> <compat_version> <bit_depth> <pb> <mb> <kb> <channels> <max_run> <max_frame_bytes> <avg_bitrate> <sample_rate>`.
fn parse_fmtp(fmtp: &str) -> Option<AlacConfig> {
    let vals: Vec<&str> = fmtp.split(' ').collect();
    if vals.len() < 12 {
        return None;
    }
    // Every field must be a valid integer — a non-numeric field is malformed
    // input and must be rejected, not silently coerced to 0 (a 0 here yields a
    // zero-size audio buffer and a 0-channel decoder downstream).
    let p = |i: usize| vals[i].parse::<u32>().ok();
    let frame_length = p(1)?;
    let compatible_version = u8::try_from(p(2)?).ok()?;
    let bit_depth = u8::try_from(p(3)?).ok()?;
    let pb = u8::try_from(p(4)?).ok()?;
    let mb = u8::try_from(p(5)?).ok()?;
    let kb = u8::try_from(p(6)?).ok()?;
    let num_channels = u8::try_from(p(7)?).ok()?;
    let max_run = u16::try_from(p(8)?).ok()?;
    let max_frame_bytes = p(9)?;
    let avg_bit_rate = p(10)?;
    let sample_rate = p(11)?;
    // Reject configs that would produce degenerate buffers / decoder state.
    if frame_length == 0
        || frame_length > 16_384
        || !matches!(bit_depth, 16 | 20 | 24 | 32)
        || num_channels == 0
        || num_channels > 8
        || !(8_000..=384_000).contains(&sample_rate)
    {
        return None;
    }
    Some(AlacConfig {
        frame_length,
        compatible_version,
        bit_depth,
        pb,
        mb,
        kb,
        num_channels,
        max_run,
        max_frame_bytes,
        avg_bit_rate,
        sample_rate,
    })
}

/// Build the 48-byte decoder info block expected by `AlacDecoder::set_info`.
/// Layout matches the ALACSpecificConfig in the Apple ALAC reference decoder.
fn build_decoder_info(config: &AlacConfig) -> [u8; 48] {
    let mut info = [0u8; 48];
    info[24..28].copy_from_slice(&config.frame_length.to_be_bytes());
    info[28] = config.compatible_version;
    info[29] = config.bit_depth;
    info[30] = config.pb;
    info[31] = config.mb;
    info[32] = config.kb;
    info[33] = config.num_channels;
    info[34..36].copy_from_slice(&config.max_run.to_be_bytes());
    info[36..40].copy_from_slice(&config.max_frame_bytes.to_be_bytes());
    info[40..44].copy_from_slice(&config.avg_bit_rate.to_be_bytes());
    info[44..48].copy_from_slice(&config.sample_rate.to_be_bytes());
    info
}

/// Circular RTP packet buffer with decrypt-on-queue and ALAC decode.
///
/// Packets are inserted by [`queue`](Self::queue) and consumed by
/// [`dequeue`](Self::dequeue). The buffer holds a fixed number of
/// frames. Sequence number wrapping is handled correctly.
///
/// # Audio pipeline
///
/// ```text
/// RTP packet → AES-128-CBC decrypt → ALAC decode → S16 → f32 → buffer slot
/// ```
pub struct RaopBuffer {
    aeskey: [u8; RAOP_AESKEY_LEN],
    aesiv: [u8; RAOP_AESIV_LEN],
    alac_config: AlacConfig,
    alac: AlacDecoder,
    is_empty: bool,
    /// Sequence number of the next frame to dequeue (oldest buffered).
    first_seqnum: u16,
    /// Sequence number of the newest buffered frame.
    last_seqnum: u16,
    entries: Vec<BufferEntry>,
    /// Number of f32 samples in one decoded audio frame.
    audio_buffer_size: usize,
}

impl RaopBuffer {
    /// Create a new buffer from SDP parameters and AES session keys.
    ///
    /// `fmtp` is parsed to determine ALAC frame size, channel count, and sample rate.
    /// The ALAC decoder is initialized immediately.
    ///
    /// Returns `None` if the (peer-supplied) `fmtp` attribute is malformed.
    pub fn new(
        _rtpmap: &str,
        fmtp: &str,
        aes_key: &[u8; RAOP_AESKEY_LEN],
        aes_iv: &[u8; RAOP_AESIV_LEN],
    ) -> Option<Self> {
        let config = parse_fmtp(fmtp)?;
        // ALAC outputs S16LE; we convert to F32 (one f32 per sample).
        let s16_buffer_size = usize::try_from(config.frame_length)
            .ok()?
            .checked_mul(usize::from(config.num_channels))?
            .checked_mul(usize::from(config.bit_depth))?
            .checked_div(8)?;
        let audio_buffer_size = s16_buffer_size / 2; // num samples
        let decoded_buffer_bytes = audio_buffer_size
            .checked_mul(RAOP_BUFFER_LENGTH)?
            .checked_mul(std::mem::size_of::<f32>())?;
        if audio_buffer_size == 0 || decoded_buffer_bytes > MAX_DECODED_BUFFER_BYTES {
            return None;
        }

        let mut alac = AlacDecoder::new(config.bit_depth as i32, config.num_channels as i32);
        let decoder_info = build_decoder_info(&config);
        alac.set_info(&decoder_info);

        let entries = (0..RAOP_BUFFER_LENGTH)
            .map(|_| BufferEntry {
                available: false,
                flags: 0,
                entry_type: 0,
                seqnum: 0,
                timestamp: 0,
                ssrc: 0,
                audio_buffer: vec![0.0f32; audio_buffer_size],
                audio_buffer_len: 0,
            })
            .collect();

        Some(Self {
            aeskey: *aes_key,
            aesiv: *aes_iv,
            alac_config: config,
            alac,
            is_empty: true,
            first_seqnum: 0,
            last_seqnum: 0,
            entries,
            audio_buffer_size,
        })
    }

    /// Returns the ALAC configuration parsed from the SDP fmtp attribute.
    pub(crate) fn config(&self) -> &AlacConfig {
        &self.alac_config
    }

    /// Return contiguous missing sequence runs still inside the live playout
    /// window. The caller can request these packets again before their RTP
    /// presentation time is reached.
    pub(crate) fn missing_runs(&self, maximum_runs: usize) -> Vec<(u16, u16)> {
        if self.is_empty || maximum_runs == 0 {
            return Vec::new();
        }
        let mut runs = Vec::new();
        let mut sequence = self.first_seqnum;
        let mut run_start = None;
        let mut run_count = 0_u16;
        loop {
            let entry = &self.entries[sequence as usize % RAOP_BUFFER_LENGTH];
            let available = entry.available && entry.seqnum == sequence;
            if available {
                if let Some(start) = run_start.take() {
                    runs.push((start, run_count));
                    if runs.len() >= maximum_runs {
                        break;
                    }
                    run_count = 0;
                }
            } else if run_start.is_none() {
                run_start = Some(sequence);
                run_count = 1;
            } else {
                run_count = run_count.saturating_add(1);
            }

            if sequence == self.last_seqnum {
                if let Some(start) = run_start {
                    runs.push((start, run_count));
                }
                break;
            }
            sequence = sequence.wrapping_add(1);
        }
        runs
    }

    /// Queue an RTP packet: decrypt, decode ALAC, convert to f32, store in buffer.
    ///
    /// Returns 1 on success, 0 if duplicate/stale, -1 if packet is malformed.
    /// If the sequence number is far ahead of the current window, the buffer is
    /// flushed to avoid stalling on lost packets.
    pub fn queue(&mut self, data: &[u8], use_seqnum: bool) -> i32 {
        let datalen = data.len();
        if !(12..=RAOP_PACKET_LEN).contains(&datalen) {
            return -1;
        }

        // Extract sequence number from RTP header bytes 2-3 (big-endian).
        let seqnum = if use_seqnum {
            ((data[2] as u16) << 8) | data[3] as u16
        } else {
            self.first_seqnum
        };

        // Drop packets older than our current window.
        if !self.is_empty && seqnum_cmp(seqnum, self.first_seqnum) < 0 {
            return 0;
        }
        // If too far ahead, flush the buffer to resync.
        if seqnum_cmp(seqnum, self.first_seqnum.wrapping_add(RAOP_BUFFER_LENGTH as u16)) >= 0 {
            self.flush(seqnum as i32);
        }

        let idx = seqnum as usize % RAOP_BUFFER_LENGTH;
        // Skip exact duplicates.
        if self.entries[idx].available && seqnum_cmp(self.entries[idx].seqnum, seqnum) == 0 {
            return 0;
        }

        // Parse RTP header fields.
        self.entries[idx].flags = data[0];
        self.entries[idx].entry_type = data[1];
        self.entries[idx].seqnum = seqnum;
        self.entries[idx].timestamp = u32::from_be_bytes([data[4], data[5], data[6], data[7]]);
        self.entries[idx].ssrc = u32::from_be_bytes([data[8], data[9], data[10], data[11]]);
        self.entries[idx].available = true;

        // AES-128-CBC decrypt: only full 16-byte blocks are encrypted,
        // trailing bytes (< 16) are sent in the clear.
        let payload = &data[12..];
        let encrypted_len = (payload.len() / 16) * 16;
        let mut packet_buf = vec![0u8; payload.len()];

        if encrypted_len > 0 {
            let decryptor = Aes128CbcDec::new((&self.aeskey).into(), (&self.aesiv).into());
            let mut encrypted = payload[..encrypted_len].to_vec();
            decryptor
                .decrypt_padded::<aes::cipher::block_padding::NoPadding>(&mut encrypted)
                .unwrap_or(&[]);
            packet_buf[..encrypted_len].copy_from_slice(&encrypted);
        }
        packet_buf[encrypted_len..].copy_from_slice(&payload[encrypted_len..]);

        // ALAC decode → S16LE, then convert to f32 samples.
        let mut s16_buf = vec![0u8; self.audio_buffer_size * 2];
        let output_size = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            self.alac.decode_frame(&packet_buf, &mut s16_buf)
        }))
        .unwrap_or(0);
        if output_size == 0 {
            return 0;
        }
        let num_samples = output_size / 2;
        for i in 0..num_samples {
            let s = i16::from_le_bytes([s16_buf[i * 2], s16_buf[i * 2 + 1]]);
            self.entries[idx].audio_buffer[i] = s as f32 / 32768.0;
        }
        self.entries[idx].audio_buffer_len = num_samples;

        // Update buffer window.
        if self.is_empty {
            self.first_seqnum = seqnum;
            self.last_seqnum = seqnum;
            self.is_empty = false;
        }
        if seqnum_cmp(seqnum, self.last_seqnum) > 0 {
            self.last_seqnum = seqnum;
        }
        1
    }

    /// Dequeue the next frame in sequence order.
    ///
    /// Returns the decoded f32 audio samples, or `None` if the buffer is empty.
    /// A missing frame is replaced with silence once its RTP presentation
    /// timestamp becomes due. Before that instant, the normal sender
    /// look-ahead still leaves time for a retransmitted packet to arrive.
    pub fn dequeue(&mut self, no_resend: bool) -> Option<&[f32]> {
        let (_, samples) = self.dequeue_with_timestamp(no_resend)?;
        Some(samples)
    }

    /// RTP timestamp of the next frame that [`dequeue`](Self::dequeue) would
    /// return, without consuming it.
    ///
    /// When a missing packet must be replaced with silence, infer its
    /// timestamp from the next available frame. AirPlay ALAC packets advance
    /// by the negotiated frame length.
    pub(crate) fn next_timestamp(&self, _no_resend: bool) -> Option<u32> {
        let buflen = seqnum_cmp(self.last_seqnum, self.first_seqnum) as i32 + 1;
        if self.is_empty || buflen <= 0 {
            return None;
        }

        let idx = self.first_seqnum as usize % RAOP_BUFFER_LENGTH;
        if self.entries[idx].available {
            return Some(self.entries[idx].timestamp);
        }

        // Infer the missing frame's presentation time from the next packet.
        // The RTP scheduler will keep it queued until that time, giving any
        // retransmit the entire sender look-ahead window to arrive. Waiting
        // until this enlarged four-second buffer is completely full would
        // otherwise turn one lost packet into a multi-second audio stall.
        let frame_length = self.alac_config.frame_length;
        (1..buflen as usize).find_map(|distance| {
            let sequence = self.first_seqnum.wrapping_add(distance as u16);
            let entry = &self.entries[sequence as usize % RAOP_BUFFER_LENGTH];
            (entry.available && entry.seqnum == sequence)
                .then(|| entry.timestamp.wrapping_sub(frame_length.wrapping_mul(distance as u32)))
        })
    }

    /// Dequeue the next frame together with its RTP presentation timestamp.
    pub(crate) fn dequeue_with_timestamp(&mut self, no_resend: bool) -> Option<(u32, &[f32])> {
        let timestamp = self.next_timestamp(no_resend)?;
        let idx = self.first_seqnum as usize % RAOP_BUFFER_LENGTH;

        self.first_seqnum = self.first_seqnum.wrapping_add(1);

        // Substitute silence for missing frames.
        if !self.entries[idx].available {
            let size = self.audio_buffer_size;
            self.entries[idx].audio_buffer[..size].fill(0.0);
            self.entries[idx].audio_buffer_len = size;
        }
        self.entries[idx].available = false;
        let len = self.entries[idx].audio_buffer_len;
        self.entries[idx].audio_buffer_len = 0;
        Some((timestamp, &self.entries[idx].audio_buffer[..len]))
    }

    /// Flush the buffer, discarding all queued frames.
    ///
    /// If `next_seq` is a valid 16-bit value (0..=0xFFFF), the buffer resets
    /// to expect that sequence number next. Otherwise the buffer is fully emptied.
    pub fn flush(&mut self, next_seq: i32) {
        for entry in &mut self.entries {
            entry.available = false;
            entry.audio_buffer_len = 0;
        }
        if !(0..=0xffff).contains(&next_seq) {
            self.is_empty = true;
        } else {
            self.first_seqnum = next_seq as u16;
            self.last_seqnum = (next_seq as u16).wrapping_sub(1);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{RAOP_BUFFER_LENGTH, RaopBuffer};

    fn buffer() -> RaopBuffer {
        RaopBuffer::new("96 352", "96 352 0 16 40 10 14 2 255 0 0 44100", &[0; 16], &[0; 16])
            .expect("valid ALAC configuration")
    }

    #[test]
    fn classic_window_covers_normal_airplay_lookahead() {
        assert!(RAOP_BUFFER_LENGTH * 352 > 44_100 * 4);
    }

    #[test]
    fn malformed_fmtp_cannot_amplify_the_lookahead_allocation() {
        assert!(
            RaopBuffer::new(
                "96 4294967295",
                "96 4294967295 0 16 40 10 14 2 255 0 0 44100",
                &[0; 16],
                &[0; 16],
            )
            .is_none()
        );
        assert!(
            RaopBuffer::new(
                "96 16384",
                "96 16384 0 32 40 10 14 8 255 0 0 384000",
                &[0; 16],
                &[0; 16],
            )
            .is_none(),
            "valid scalar fields that exceed the total memory cap must fail closed"
        );
        assert!(
            RaopBuffer::new("96 352", "96 352 0 272 40 10 14 258 255 0 0 44100", &[0; 16], &[0; 16],).is_none(),
            "narrow ALAC fields must not silently truncate"
        );
    }

    #[test]
    fn missing_frame_timestamp_is_inferred_from_next_packet() {
        let mut buffer = buffer();
        buffer.is_empty = false;
        buffer.first_seqnum = 10;
        buffer.last_seqnum = 11;
        let next = &mut buffer.entries[11 % RAOP_BUFFER_LENGTH];
        next.available = true;
        next.seqnum = 11;
        next.timestamp = 50_352;

        assert_eq!(buffer.next_timestamp(true), Some(50_000));
        assert_eq!(buffer.next_timestamp(false), Some(50_000));
        let (timestamp, samples) = buffer
            .dequeue_with_timestamp(true)
            .expect("missing packet should become timed silence");
        assert_eq!(timestamp, 50_000);
        assert_eq!(samples.len(), buffer.audio_buffer_size);
    }

    #[test]
    fn missing_runs_group_gaps_and_handle_sequence_wraparound() {
        let mut buffer = buffer();
        buffer.is_empty = false;
        buffer.first_seqnum = u16::MAX - 1;
        buffer.last_seqnum = 2;
        for sequence in [u16::MAX - 1, 1, 2] {
            let entry = &mut buffer.entries[sequence as usize % RAOP_BUFFER_LENGTH];
            entry.available = true;
            entry.seqnum = sequence;
        }

        assert_eq!(buffer.missing_runs(8), vec![(u16::MAX, 2)]);
    }
}
