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

/// Decodes G.711 PCMU (PT 0) or PCMA (PT 8) payload bytes back into mono 8 kHz `i16` PCM samples.
#[must_use]
pub fn decode_g711(payload: &[u8], payload_type: u8) -> Vec<i16> {
    match payload_type {
        8 => payload.iter().copied().map(alaw_to_linear).collect(),
        _ => payload.iter().copied().map(ulaw_to_linear).collect(),
    }
}

/// ITU-T G.711 mu-law expansion (8-bit PCMU -> 16-bit linear PCM).
#[must_use]
pub fn ulaw_to_linear(u_val: u8) -> i16 {
    const BIAS: i32 = 0x84;
    let u = !u_val;
    let t = (i32::from(u & 0x0F) << 3) + BIAS;
    let shifted = t << ((u & 0x70) >> 4);
    let pcm = if (u & 0x80) != 0 {
        BIAS - shifted
    } else {
        shifted - BIAS
    };
    pcm.clamp(i32::from(i16::MIN), i32::from(i16::MAX)) as i16
}

/// ITU-T G.711 A-law expansion (8-bit PCMA -> 16-bit linear PCM).
#[must_use]
pub fn alaw_to_linear(a_val: u8) -> i16 {
    let a = a_val ^ 0x55;
    let mut t = i32::from(a & 0x0F) << 4;
    let seg = (a & 0x70) >> 4;
    match seg {
        0 => t += 8,
        1 => t += 0x108,
        _ => {
            t += 0x108;
            t <<= seg - 1;
        }
    }
    let pcm = if (a & 0x80) != 0 { t } else { -t };
    pcm.clamp(i32::from(i16::MIN), i32::from(i16::MAX)) as i16
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

        let dec_u = decode_g711(&pcmu.data, 0);
        let dec_a = decode_g711(&pcma.data, 8);
        assert_eq!(dec_u.len(), 5);
        assert_eq!(dec_a.len(), 5);
        assert!(dec_u[0].abs() < 32);
        assert!(dec_u[3] > 14000 && dec_u[4] < -14000);
        assert!(dec_a[3] > 14000 && dec_a[4] < -14000);
    }
}
