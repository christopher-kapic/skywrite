//! Color metadata reporting for inspection. Nothing here guesses an encoding.
//!
//! FFmpeg's decoded fields (`color_transfer`, `color_primaries`, ...) come from
//! its public label tables. MXF files can also carry private, camera-specific
//! labels that FFmpeg leaves undecoded; a bounded header scan reports those raw
//! so an operator-confirmed value can be declared in the edit (`sources[].color`).
use anyhow::{Context, Result};
use serde_json::{Map, Value, json};
use std::{io::Read, path::Path, time::Duration};

/// Header metadata normally precedes essence within the first few MiB.
const MXF_SCAN_LIMIT: u64 = 32 * 1024 * 1024;
const MAX_TAGS: usize = 64;
const MAX_TAG_BYTES: usize = 512;
const MAX_PRIVATE_LABELS: usize = 64;

const UL_PREFIX: [u8; 4] = [0x06, 0x0E, 0x2B, 0x34];
/// Property ULs (byte 7, the registry version, is ignored when matching).
const TRANSFER: [u8; 16] = [
    0x06, 0x0E, 0x2B, 0x34, 0x01, 0x01, 0x01, 0x02, 0x04, 0x01, 0x02, 0x01, 0x01, 0x01, 0x02, 0x00,
];
const PRIMARIES: [u8; 16] = [
    0x06, 0x0E, 0x2B, 0x34, 0x01, 0x01, 0x01, 0x09, 0x04, 0x01, 0x02, 0x01, 0x01, 0x06, 0x01, 0x00,
];
const CODING_EQUATIONS: [u8; 16] = [
    0x06, 0x0E, 0x2B, 0x34, 0x01, 0x01, 0x01, 0x02, 0x04, 0x01, 0x02, 0x01, 0x01, 0x03, 0x01, 0x00,
];
const PRIMER_PACK: [u8; 14] = [
    0x06, 0x0E, 0x2B, 0x34, 0x02, 0x05, 0x01, 0x01, 0x0D, 0x01, 0x02, 0x01, 0x01, 0x05,
];

/// Inspect-time color report for every video stream.
pub fn report(path: &Path, format_name: &str) -> Result<Value> {
    let mut args = crate::process::args(&[
        "-v",
        "error",
        "-protocol_whitelist",
        "file,pipe",
        "-select_streams",
        "v",
        "-show_entries",
        "stream=index,pix_fmt,color_range,color_space,color_transfer,color_primaries,chroma_location,bits_per_raw_sample,field_order:stream_tags:format_tags",
        "-of",
        "json",
    ]);
    args.push(path.as_os_str().to_owned());
    let out = crate::process::run("ffprobe", &args, Duration::from_secs(30), 4 * 1024 * 1024)?;
    let raw: Value =
        serde_json::from_slice(&out.stdout).context("ffprobe returned invalid JSON")?;
    let streams = raw["streams"]
        .as_array()
        .map(|streams| {
            streams
                .iter()
                .map(|s| {
                    let mut entry = Map::new();
                    for key in [
                        "index",
                        "pix_fmt",
                        "color_range",
                        "color_space",
                        "color_transfer",
                        "color_primaries",
                        "chroma_location",
                        "bits_per_raw_sample",
                        "field_order",
                    ] {
                        entry.insert(key.into(), s.get(key).cloned().unwrap_or(Value::Null));
                    }
                    entry.insert("tags".into(), bounded_tags(&s["tags"]));
                    Value::Object(entry)
                })
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();
    let mxf = if format_name.split(',').any(|f| f == "mxf") {
        Some(scan_mxf_file(path)?)
    } else {
        None
    };
    Ok(json!({
        "streams": streams,
        "format_tags": bounded_tags(&raw["format"]["tags"]),
        "mxf_header": mxf,
        "interpretation": "color_* fields are FFmpeg's decoding of standard container labels; null means not signalled or not decodable. Private camera labels are reported raw under mxf_header and are not interpreted. Visual appearance cannot establish an encoding. When the operator confirms the capture gamma/gamut, declare it in the edit as sources[].color and supply a matching sources[].lut for rendering.",
    }))
}

fn bounded_tags(tags: &Value) -> Value {
    let Some(tags) = tags.as_object() else {
        return json!({});
    };
    let mut out = Map::new();
    for (key, value) in tags.iter().take(MAX_TAGS) {
        let text = value
            .as_str()
            .map(str::to_owned)
            .unwrap_or_else(|| value.to_string());
        let mut cut = text.len().min(MAX_TAG_BYTES);
        while !text.is_char_boundary(cut) {
            cut -= 1;
        }
        out.insert(
            key.chars().take(128).collect(),
            Value::String(text[..cut].to_owned()),
        );
    }
    Value::Object(out)
}

fn scan_mxf_file(path: &Path) -> Result<Value> {
    let mut bytes = Vec::new();
    std::fs::File::open(path)?
        .take(MXF_SCAN_LIMIT)
        .read_to_end(&mut bytes)?;
    Ok(scan_mxf(&bytes))
}

/// Dotted hex grouping matching exiftool's display (4.2.2.4.4 bytes).
pub fn format_ul(ul: &[u8]) -> String {
    let hex = |range: std::ops::Range<usize>| {
        ul[range]
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect::<String>()
    };
    format!(
        "{}.{}.{}.{}.{}",
        hex(0..4),
        hex(4..6),
        hex(6..8),
        hex(8..12),
        hex(12..16)
    )
}

fn same_property(a: &[u8], b: &[u8; 16]) -> bool {
    a.len() == 16 && a[..7] == b[..7] && a[8..] == b[8..]
}

fn classify(ul: &[u8]) -> &'static str {
    if ul.len() != 16 || ul[..4] != UL_PREFIX {
        "not a SMPTE universal label"
    } else if ul[8] == 0x0E {
        "organizationally registered private label; not decodable from public registries"
    } else if ul[8] == 0x0D {
        "organizationally registered public-use label"
    } else {
        "SMPTE-registered label"
    }
}

fn ber_length(bytes: &[u8], at: usize) -> Option<(u64, usize)> {
    let first = *bytes.get(at)?;
    if first < 0x80 {
        return Some((first as u64, 1));
    }
    let count = (first & 0x7F) as usize;
    if count == 0 || count > 8 {
        return None;
    }
    let mut length = 0u64;
    for i in 0..count {
        length = (length << 8) | *bytes.get(at + 1 + i)? as u64;
    }
    Some((length, 1 + count))
}

/// Walk header KLV packets and report color-related labels from local sets.
pub fn scan_mxf(bytes: &[u8]) -> Value {
    let mut primer = std::collections::HashMap::<u16, [u8; 16]>::new();
    let mut descriptors = Vec::new();
    let mut private = Vec::new();
    let mut at = 0usize;
    let mut complete = false;
    let mut packets = 0usize;
    while at + 17 <= bytes.len() {
        let key = &bytes[at..at + 16];
        if key[..4] != UL_PREFIX {
            break;
        }
        // Essence container data (generic container elements) ends header metadata.
        if key[4] == 0x01 && key[8..12] == [0x0D, 0x01, 0x03, 0x01] {
            complete = true;
            break;
        }
        // A body partition also follows the header metadata.
        if key[4] == 0x02 && key[5] == 0x05 && key[13] == 0x03 {
            complete = true;
            break;
        }
        let Some((length, length_bytes)) = ber_length(bytes, at + 16) else {
            break;
        };
        let start = at + 16 + length_bytes;
        let Some(end) = start
            .checked_add(length as usize)
            .filter(|end| *end <= bytes.len())
        else {
            break;
        };
        let value = &bytes[start..end];
        packets += 1;
        if key[..14] == PRIMER_PACK && value.len() >= 8 {
            let count = u32::from_be_bytes(value[0..4].try_into().expect("4 bytes")) as usize;
            let size = u32::from_be_bytes(value[4..8].try_into().expect("4 bytes")) as usize;
            if size == 18 {
                for item in value[8..].chunks_exact(18).take(count) {
                    let tag = u16::from_be_bytes([item[0], item[1]]);
                    primer.insert(tag, item[2..18].try_into().expect("16 bytes"));
                }
            }
        } else if key[4] == 0x02 && key[5] == 0x53 {
            let mut found = Map::new();
            let mut i = 0usize;
            while i + 4 <= value.len() {
                let tag = u16::from_be_bytes([value[i], value[i + 1]]);
                let len = u16::from_be_bytes([value[i + 2], value[i + 3]]) as usize;
                let Some(item) = value.get(i + 4..i + 4 + len) else {
                    break;
                };
                let property = primer.get(&tag).copied().or(match tag {
                    0x3210 => Some(TRANSFER),
                    0x3219 => Some(PRIMARIES),
                    0x321A => Some(CODING_EQUATIONS),
                    _ => None,
                });
                let name = property.and_then(|p| {
                    if same_property(&p, &TRANSFER) {
                        Some("transfer_characteristic")
                    } else if same_property(&p, &PRIMARIES) {
                        Some("color_primaries")
                    } else if same_property(&p, &CODING_EQUATIONS) {
                        Some("coding_equations")
                    } else {
                        None
                    }
                });
                if let Some(name) = name
                    && item.len() == 16
                {
                    found.insert(
                        name.into(),
                        json!({"ul": format_ul(item), "registry": classify(item)}),
                    );
                } else if item.len() == 16
                    && item[..4] == UL_PREFIX
                    && item[4] == 0x04
                    && item[8] == 0x0E
                    && private.len() < MAX_PRIVATE_LABELS
                {
                    private.push(json!({
                        "local_tag": format!("0x{tag:04x}"),
                        "property": property.map(|p| format_ul(&p)),
                        "value": format_ul(item),
                    }));
                }
                i += 4 + len;
            }
            if !found.is_empty() {
                found.insert("set_key".into(), json!(format_ul(key)));
                descriptors.push(Value::Object(found));
            }
        }
        at = end;
    }
    json!({
        "scanned_bytes": at.min(bytes.len()),
        "packets": packets,
        "header_complete": complete,
        "descriptors": descriptors,
        "other_private_labels": private,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn klv(key: [u8; 16], value: &[u8]) -> Vec<u8> {
        let mut out = key.to_vec();
        out.push(0x83);
        out.extend_from_slice(&(value.len() as u32).to_be_bytes()[1..]);
        out.extend_from_slice(value);
        out
    }

    #[test]
    fn reports_private_transfer_labels_raw() {
        // The label a tester's Canon C80 MXF carried (per exiftool), operator-confirmed
        // as Canon Log 2; Skywrite reports it raw rather than mapping it.
        let canon: [u8; 16] = [
            0x06, 0x0E, 0x2B, 0x34, 0x04, 0x01, 0x01, 0x0D, 0x0E, 0x15, 0x00, 0x01, 0x02, 0x00,
            0x00, 0x00,
        ];
        let mut primer = Vec::new();
        primer.extend_from_slice(&1u32.to_be_bytes());
        primer.extend_from_slice(&18u32.to_be_bytes());
        primer.extend_from_slice(&0x8001u16.to_be_bytes());
        primer.extend_from_slice(&TRANSFER);
        let mut set = Vec::new();
        set.extend_from_slice(&0x8001u16.to_be_bytes());
        set.extend_from_slice(&16u16.to_be_bytes());
        set.extend_from_slice(&canon);
        let mut primer_key = [0u8; 16];
        primer_key[..14].copy_from_slice(&PRIMER_PACK);
        primer_key[14] = 0x01;
        let cdci = [
            0x06, 0x0E, 0x2B, 0x34, 0x02, 0x53, 0x01, 0x01, 0x0D, 0x01, 0x01, 0x01, 0x01, 0x01,
            0x28, 0x00,
        ];
        let essence = [
            0x06, 0x0E, 0x2B, 0x34, 0x01, 0x02, 0x01, 0x01, 0x0D, 0x01, 0x03, 0x01, 0x15, 0x01,
            0x05, 0x00,
        ];
        let mut bytes = klv(primer_key, &primer);
        bytes.extend(klv(cdci, &set));
        bytes.extend(klv(essence, b"frame"));
        let report = scan_mxf(&bytes);
        assert_eq!(report["header_complete"], true);
        let transfer = &report["descriptors"][0]["transfer_characteristic"];
        assert_eq!(transfer["ul"], "060e2b34.0401.010d.0e150001.02000000");
        assert!(transfer["registry"].as_str().unwrap().contains("private"));
        // Truncated or foreign data stops quietly.
        assert_eq!(scan_mxf(&bytes[..20])["packets"], 0);
        assert_eq!(scan_mxf(b"not mxf at all, just text")["packets"], 0);
    }
}
