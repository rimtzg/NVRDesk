//! Pure-Rust ITU-T G.711 audio encoders (`PCMU` mu-law & `PCMA` A-law) for NVR compatibility.

use bytes::Bytes;
use nvrdesk_core::{AudioCodecKind, EncodedAudioFrame};

/// Encodes mono 8 kHz `i16` PCM samples into G.711 PCMU or PCMA.
#[must_use]
pub fn encode_g711(samples: &[i16], codec: AudioCodecKind, pts_8khz: u32) -> EncodedAudioFrame {
    let mut encoded = Vec::with_capacity(samples.len());
    let payload_type = match codec {
        AudioCodecKind::Pcmu => {
            for &s in samples {
                encoded.push(linear_to_ulaw(s));
            }
            0
        }
        AudioCodecKind::Pcma => {
            for &s in samples {
                encoded.push(linear_to_alaw(s));
            }
            8
        }
    };

    EncodedAudioFrame {
        data: Bytes::from(encoded),
        pts_clock: pts_8khz,
        payload_type,
    }
}

/// ITU-T G.711 mu-law compression (16-bit linear PCM -> 8-bit PCMU).
#[must_use]
pub fn linear_to_ulaw(sample: i16) -> u8 {
    const BIAS: i32 = 0x84;
    const CLIP: i32 = 32635;

    let mut pcm = i32::from(sample);
    let sign = if pcm < 0 {
        pcm = pcm.saturating_neg();
        0x80
    } else {
        0x00
    };
    if pcm > CLIP {
        pcm = CLIP;
    }
    pcm += BIAS;

    let mut exponent = 7u8;
    let mut exp_mask = 0x4000i32;
    while exponent > 0 && (pcm & exp_mask) == 0 {
        exponent -= 1;
        exp_mask >>= 1;
    }

    let mantissa = ((pcm >> (exponent + 3)) & 0x0F) as u8;
    !(sign | (exponent << 4) | mantissa)
}

/// ITU-T G.711 A-law compression (16-bit linear PCM -> 8-bit PCMA).
#[must_use]
pub fn linear_to_alaw(sample: i16) -> u8 {
    let mut pcm = i32::from(sample);
    let mask = if pcm >= 0 {
        0xD5u8
    } else {
        pcm = pcm.saturating_neg().min(i32::from(i16::MAX));
        0x55u8
    };

    let aval = if pcm < 256 {
        (pcm >> 4) as u8
    } else {
        let mut exponent = 7u8;
        let mut exp_mask = 0x4000i32;
        while exponent > 1 && (pcm & exp_mask) == 0 {
            exponent -= 1;
            exp_mask >>= 1;
        }
        let mantissa = ((pcm >> (exponent + 3)) & 0x0F) as u8;
        (exponent << 4) | mantissa
    };

    aval ^ mask
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn encodes_pcmu_and_pcma_silence_and_peaks() {
        let samples = [0i16, 1000, -1000, 16000, -16000];
        let pcmu = encode_g711(&samples, AudioCodecKind::Pcmu, 160);
        assert_eq!(pcmu.payload_type, 0);
        assert_eq!(pcmu.data.len(), 5);
        assert_eq!(pcmu.data[0], 0xFF); // Standard G.711 PCMU silence byte

        let pcma = encode_g711(&samples, AudioCodecKind::Pcma, 320);
        assert_eq!(pcma.payload_type, 8);
        assert_eq!(pcma.data.len(), 5);
        assert_eq!(pcma.data[0], 0xD5); // Standard G.711 PCMA silence byte
    }
}
