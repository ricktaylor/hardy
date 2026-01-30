#![cfg(feature = "htb_policy")]

use core::ops::RangeInclusive;

#[cfg(feature = "serde")]
use serde::{Deserialize, Deserializer, de};

/// Configuration for the HTB (Hierarchical Token Bucket) policy.
#[derive(Debug)]
#[cfg_attr(feature = "serde", derive(Deserialize))]
pub struct HtbConfig {
    pub channels: Vec<HtbChannel>,
}

/// Represents a single prioritized channel within an HTB policy.
/// The configuration fields are now direct members of this struct.
#[derive(Debug)]
#[cfg_attr(feature = "serde", derive(Deserialize))]
pub struct HtbChannel {
    pub name: String,
    #[cfg_attr(feature = "serde", serde(deserialize_with = "deserialize_flow_labels"))]
    pub flow_labels: Vec<RangeInclusive<u32>>,
    #[cfg_attr(feature = "serde", serde(default))]
    pub default: bool,
    pub priority: usize,
    pub rate: String,
    pub ceil: String,
}

/// Custom deserializer for the `flow_labels` field.
#[cfg(feature = "serde")]
fn deserialize_flow_labels<'de, D>(
    deserializer: D,
) -> core::result::Result<Vec<RangeInclusive<u32>>, D::Error>
where
    D: Deserializer<'de>,
{
    // First, deserialize the YAML value as a simple String.
    let s: String = Deserialize::deserialize(deserializer)?;

    // Now, parse that string into the desired Vec<RangeInclusive<u32>>.
    let mut ranges = Vec::new();

    // Trim surrounding brackets `[` and `]` for convenience.
    let content = s
        .trim()
        .strip_prefix('[')
        .unwrap_or(&s)
        .strip_suffix(']')
        .unwrap_or(&s);

    for part in content.split(',') {
        let part = part.trim();
        if part.is_empty() {
            continue;
        }

        // Use `-` as the separator for ranges, e.g., "10-19".
        match part.split_once("-") {
            // Case 1: Found a range like "1-5" or "5-1"
            Some((start_str, end_str)) => {
                let a = start_str.trim().parse().map_err(de::Error::custom)?;
                let b = end_str.trim().parse().map_err(de::Error::custom)?;
                // Automatically handle inverted ranges, e.g., "10-5"
                ranges.push(a.min(b)..=a.max(b));
            }
            // Case 2: It's a single number like "7"
            None => {
                let num = part.parse().map_err(de::Error::custom)?;
                ranges.push(num..=num);
            }
        }
    }

    // --- Merge adjacent and overlapping ranges for efficiency ---

    if ranges.is_empty() {
        return Ok(ranges);
    }

    // 1. Sort the ranges by their start value.
    ranges.sort_by_key(|r| *r.start());

    // 2. Merge them into a new vector (at most as many as input ranges).
    let mut merged = Vec::with_capacity(ranges.len());
    let mut current_range = ranges.remove(0);

    for next_range in ranges {
        // Check if the next range is adjacent or overlapping.
        // The `+ 1` handles adjacency, e.g., `1..=5` and `6..=10`.
        if *next_range.start() <= current_range.end().saturating_add(1) {
            // If so, extend the current range to encompass the next one.
            current_range = *current_range.start()..=*current_range.end().max(next_range.end());
        } else {
            // Otherwise, the current range is finished. Push it to the results
            // and start a new current range.
            merged.push(current_range);
            current_range = next_range;
        }
    }
    // Add the last processed range.
    merged.push(current_range);

    Ok(merged)
}
