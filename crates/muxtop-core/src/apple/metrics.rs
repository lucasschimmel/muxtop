//! Pure derivations for the Apple Silicon backend: no FFI, no I/O.
//!
//! # Why this module is not `cfg`-gated
//!
//! Everything Apple-specific about the backend that can be *wrong* lives here
//! — the DVFS blob layout, the energy-unit table, the residency weighting —
//! and none of it needs macOS to execute. Keeping it un-gated means its tests
//! run on the Linux and Windows CI legs too, exactly as `amd_engine`'s parser
//! tests do. The v0.4.2 Windows break was a reminder of how fast platform code
//! rots when only one leg exercises it.
//!
//! The FFI that feeds these functions is in `ioregistry.rs` and `ioreport.rs`
//! and *is* gated, because it links `IOKit` and dlopens a Darwin-only library.

use std::time::Duration;

/// One entry of the GPU DVFS table: a performance state's clock in MHz.
///
/// Index 0 is the parked state and is always 0 MHz — Apple's table encodes it
/// with a real voltage but no frequency. `IOReport` names the active states
/// `P1`, `P2`, … which index straight into this vector.
pub type DvfsTable = Vec<u32>;

/// Bytes per `(frequency, voltage)` pair in a `voltage-states*` blob.
const DVFS_ENTRY_LEN: usize = 8;

/// Upper bound on a plausible GPU clock, in MHz.
///
/// The blob is an undocumented device-tree property, so a layout change on a
/// future chip would decode as garbage rather than fail. Anything above this
/// is treated as "we no longer understand this table" and the whole table is
/// rejected — reporting `—` for the clock is recoverable, reporting a
/// fictional number next to real ones is not. Apple's fastest GPU state to
/// date is under 1.6 GHz, so the bound leaves generous headroom while still
/// catching a misread field.
const MAX_PLAUSIBLE_CLOCK_MHZ: u32 = 4_000;

/// Decode a `voltage-states9`-style blob into per-state clocks in MHz.
///
/// The layout is a flat array of `(u32 frequency_hz, u32 voltage_mv)` pairs in
/// little-endian order, lowest state first. Only the frequency half is of any
/// use here; the voltages are carried along by the firmware for the power
/// manager.
///
/// Returns `None` when the blob is empty, is not a whole number of pairs, or
/// decodes to a clock no GPU could run at. The last of those is the reason the
/// bound exists: the property is undocumented, so a layout change on a future
/// chip would decode as garbage rather than fail, and a rejected table costs
/// the CLK column where an accepted one would print a fictional number next to
/// real ones.
pub fn parse_dvfs_table(blob: &[u8]) -> Option<DvfsTable> {
    if blob.is_empty() || !blob.len().is_multiple_of(DVFS_ENTRY_LEN) {
        return None;
    }

    let mut table = Vec::with_capacity(blob.len() / DVFS_ENTRY_LEN);
    for pair in blob.as_chunks::<DVFS_ENTRY_LEN>().0 {
        let hz = u32::from_le_bytes([pair[0], pair[1], pair[2], pair[3]]);
        // Round to nearest rather than truncating: Apple's entries are exact
        // multiples of 1 MHz on every chip seen so far, but a 1 337.999 MHz
        // state should read 1338, not 1337. Widened to u64 first — the
        // rounding term overflows u32 on a garbage entry, which is precisely
        // the case the plausibility check below exists to reject.
        let mhz = ((u64::from(hz) + 500_000) / 1_000_000) as u32;
        if mhz > MAX_PLAUSIBLE_CLOCK_MHZ {
            return None;
        }
        table.push(mhz);
    }
    Some(table)
}

/// Map an `IOReport` performance-state name to its index in a [`DvfsTable`].
///
/// The GPU channel names its states `OFF` for the parked state and `P1`…`P13`
/// for the active ones. Anything else is a state this build does not
/// understand — a new chip family, or a channel we subscribed to by mistake —
/// and is deliberately *not* guessed at.
pub fn state_index(name: &str) -> Option<usize> {
    if name.eq_ignore_ascii_case("off") {
        return Some(0);
    }
    let digits = name.strip_prefix('P').or_else(|| name.strip_prefix('p'))?;
    if digits.is_empty() || !digits.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    digits.parse().ok()
}

/// Residency-weighted average clock over the states the GPU actually ran in.
///
/// `residencies` is the per-state tick delta over one collection interval, as
/// `IOReport` reports it. The average deliberately excludes the parked state:
/// a GPU that spent 90 % of the second clock-gated and 10 % at 1 338 MHz was
/// running at 1 338 MHz when it ran, and averaging the zero in would report a
/// clock the hardware never used.
///
/// Returns:
/// * `None` when nothing is known — no counter advanced, or the GPU spent time
///   in a state the DVFS table cannot name (which means the blob layout no
///   longer matches the hardware and the decoded clocks cannot be trusted).
/// * `Some(0)` when the counters advanced but every tick was parked. This is
///   the honest reading of a fully idle GPU, and matches the tab's contract
///   that `0` means zero while `—` means unknown.
pub fn residency_weighted_clock_mhz(
    residencies: &[(String, u64)],
    table: &DvfsTable,
) -> Option<u32> {
    let mut weighted = 0u128;
    let mut active_ticks = 0u128;
    let mut known_ticks = 0u128;

    for (name, ticks) in residencies {
        let clock = match state_index(name).and_then(|index| table.get(index)) {
            Some(clock) => *clock,
            // The channel pads its state array beyond the states the chip
            // actually has: an M3 reports P1..P15 against a 14-entry table,
            // with the surplus permanently at zero. An unnamed state that
            // never ran costs nothing and is skipped. One that *did* run means
            // the two sources genuinely disagree about this chip, and guessing
            // a clock from a mismatched table is worse than admitting we do
            // not know.
            None if *ticks == 0 => continue,
            None => return None,
        };
        known_ticks += u128::from(*ticks);
        if clock > 0 {
            weighted += u128::from(*ticks) * u128::from(clock);
            active_ticks += u128::from(*ticks);
        }
    }

    if known_ticks == 0 {
        return None;
    }
    if active_ticks == 0 {
        return Some(0);
    }
    u32::try_from(weighted / active_ticks).ok()
}

/// Fraction of an interval the GPU spent outside the parked state, 0–100 %.
///
/// A second opinion on utilisation, derived from the same residency counters
/// as the clock. The Devices table shows the driver's own `Device Utilization
/// %` instead — this exists so the engine can fall back when the IORegistry
/// figure is missing, and it is intentionally *not* averaged with it: the two
/// measure different things (work submitted versus time unparked) and
/// blending them would produce a number neither source stands behind.
pub fn active_residency_pct(residencies: &[(String, u64)], table: &DvfsTable) -> Option<f32> {
    let mut active = 0u128;
    let mut total = 0u128;

    for (name, ticks) in residencies {
        // Same padding rule as the clock above: an unnamed state that never
        // ran is skipped, one that ran invalidates the whole reading.
        let clock = match state_index(name).and_then(|index| table.get(index)) {
            Some(clock) => *clock,
            None if *ticks == 0 => continue,
            None => return None,
        };
        total += u128::from(*ticks);
        if clock > 0 {
            active += u128::from(*ticks);
        }
    }

    if total == 0 {
        return None;
    }
    Some((active as f64 / total as f64 * 100.0) as f32)
}

/// Convert an `IOReport` energy delta into average power over the interval.
///
/// The GPU energy channel reports an accumulating counter whose unit is
/// carried in the channel's own metadata rather than fixed by the API — the
/// same machine exposes `GPU Energy` in nanojoules and `GPU` in millijoules.
/// Hard-coding either would be off by a factor of a million on the other.
///
/// Returns `None` for an unrecognised unit, a zero-length interval, or a
/// negative delta (a counter reset across the interval, which yields no usable
/// average).
pub fn energy_delta_to_watts(delta: i64, unit: &str, elapsed: Duration) -> Option<f32> {
    if delta < 0 {
        return None;
    }
    let seconds = elapsed.as_secs_f64();
    if seconds <= 0.0 {
        return None;
    }
    let joules = delta as f64 * energy_unit_joules(unit)?;
    Some((joules / seconds) as f32)
}

/// Joules per unit for the energy-unit labels `IOReport` uses.
///
/// `µJ` appears with both the micro sign (U+00B5) and the Greek mu (U+03BC)
/// depending on the channel, and plain `uJ` on others — all three are the same
/// unit and all three are accepted.
fn energy_unit_joules(unit: &str) -> Option<f64> {
    match unit.trim() {
        "nJ" => Some(1e-9),
        "uJ" | "µJ" | "μJ" => Some(1e-6),
        "mJ" => Some(1e-3),
        "J" => Some(1.0),
        _ => None,
    }
}

/// Clamp a driver-reported percentage into the range the gauges assume.
///
/// `PerformanceStatistics` is a driver-owned dictionary; nothing in the API
/// contract stops it from reporting 101 %, and a gauge fed 101 % would paint
/// outside its own bar.
pub fn clamp_pct(raw: i64) -> f32 {
    (raw as f32).clamp(0.0, 100.0)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `voltage-states9` as read from a MacBook Air M3 (macOS 26.5.1), the
    /// host this backend was developed against. Fourteen `(Hz, mV)` pairs:
    /// a parked entry plus P1…P13 running 338 MHz to 1 338 MHz.
    const M3_DVFS_BLOB: &[u8] = &[
        0x00, 0x00, 0x00, 0x00, 0x7d, 0x00, 0x00, 0x00, // parked, 125 mV
        0x80, 0x78, 0x25, 0x14, 0x71, 0x02, 0x00, 0x00, // P1  338 MHz
        0x80, 0xee, 0xd5, 0x24, 0xa8, 0x02, 0x00, 0x00, // P2  618 MHz
        0x00, 0xff, 0x71, 0x2f, 0xd5, 0x02, 0x00, 0x00, // P3  796 MHz
        0x00, 0x59, 0xd4, 0x31, 0xfd, 0x02, 0x00, 0x00, // P4  836 MHz
        0x00, 0x28, 0x50, 0x37, 0xfd, 0x02, 0x00, 0x00, // P5  928 MHz
        0x00, 0x5e, 0xbe, 0x38, 0x2f, 0x03, 0x00, 0x00, // P6  952 MHz
        0x00, 0x48, 0xf1, 0x3e, 0x2f, 0x03, 0x00, 0x00, // P7  1056 MHz
        0x40, 0x81, 0xc3, 0x3e, 0x6b, 0x03, 0x00, 0x00, // P8  1053 MHz
        0x80, 0xc8, 0xbc, 0x45, 0x6b, 0x03, 0x00, 0x00, // P9  1170 MHz
        0x00, 0x20, 0xaa, 0x44, 0x93, 0x03, 0x00, 0x00, // P10 1152 MHz
        0x80, 0xbb, 0x2c, 0x4c, 0x93, 0x03, 0x00, 0x00, // P11 1278 MHz
        0x00, 0x95, 0xc3, 0x47, 0xac, 0x03, 0x00, 0x00, // P12 1204 MHz
        0x80, 0x42, 0xc0, 0x4f, 0xac, 0x03, 0x00, 0x00, // P13 1338 MHz
    ];

    fn m3_table() -> DvfsTable {
        parse_dvfs_table(M3_DVFS_BLOB).expect("the captured M3 blob must decode")
    }

    fn residency(pairs: &[(&str, u64)]) -> Vec<(String, u64)> {
        pairs.iter().map(|(n, t)| ((*n).into(), *t)).collect()
    }

    // ---- DVFS table -------------------------------------------------------

    #[test]
    fn dvfs_blob_decodes_to_the_states_the_hardware_reports() {
        let table = m3_table();
        assert_eq!(table.len(), 14, "parked state plus P1..P13");
        assert_eq!(table[0], 0, "index 0 is the parked state");
        assert_eq!(table[1], 338);
        assert_eq!(table[13], 1338);
    }

    #[test]
    fn dvfs_table_is_not_assumed_monotonic() {
        // P8 (1053 MHz) sits *below* P7 (1056 MHz) on the M3: the states are
        // ordered by voltage, not by clock. Any code that sorted or bisected
        // this table would mis-attribute a residency, so the decode must
        // preserve the firmware's order verbatim.
        let table = m3_table();
        assert!(table[8] < table[7], "P8 really is slower than P7 here");
    }

    #[test]
    fn dvfs_blob_rejects_a_partial_pair() {
        assert_eq!(parse_dvfs_table(&[0x00, 0x01, 0x02]), None);
    }

    #[test]
    fn dvfs_blob_rejects_an_empty_property() {
        assert_eq!(parse_dvfs_table(&[]), None);
    }

    #[test]
    fn dvfs_blob_rejects_an_implausible_clock() {
        // A layout change on a future chip decodes as garbage rather than
        // failing outright. Rejecting the whole table costs the CLK column;
        // accepting it would print a fictional number next to real ones.
        let blob = [0xff, 0xff, 0xff, 0xff, 0x00, 0x00, 0x00, 0x00];
        assert_eq!(parse_dvfs_table(&blob), None);
    }

    // ---- state names ------------------------------------------------------

    #[test]
    fn state_names_map_to_table_indices() {
        assert_eq!(state_index("OFF"), Some(0));
        assert_eq!(state_index("off"), Some(0));
        assert_eq!(state_index("P1"), Some(1));
        assert_eq!(state_index("P13"), Some(13));
    }

    #[test]
    fn unknown_state_names_are_not_guessed_at() {
        // `SW_P1` belongs to the *Software* performance-state channel. If it
        // ever reached this function the subscription picked up the wrong
        // subgroup, and silently folding it in would double-count residency.
        assert_eq!(state_index("SW_P1"), None);
        assert_eq!(state_index("IDLE"), None);
        assert_eq!(state_index("P"), None);
        assert_eq!(state_index("Px"), None);
        assert_eq!(state_index(""), None);
    }

    // ---- residency-weighted clock ----------------------------------------

    #[test]
    fn clock_excludes_the_parked_state_from_the_average() {
        // Nine tenths parked, one tenth at 1 338 MHz. The GPU ran at
        // 1 338 MHz when it ran; averaging the parked ticks in would report
        // 134 MHz, a clock the hardware never used.
        let table = m3_table();
        let r = residency(&[("OFF", 900), ("P13", 100)]);
        assert_eq!(residency_weighted_clock_mhz(&r, &table), Some(1338));
    }

    #[test]
    fn clock_weights_active_states_by_residency() {
        let table = m3_table();
        // 338 MHz for 3 ticks, 1338 MHz for 1 → (3*338 + 1338) / 4 = 588.
        let r = residency(&[("P1", 3), ("P13", 1)]);
        assert_eq!(residency_weighted_clock_mhz(&r, &table), Some(588));
    }

    #[test]
    fn clock_is_zero_when_the_gpu_was_parked_all_interval() {
        // Counters advanced and every tick was parked: the GPU really was at
        // 0 MHz. `Some(0)` and `None` are different claims and the tab's whole
        // contract rests on not confusing them.
        let table = m3_table();
        let r = residency(&[("OFF", 24_000_000)]);
        assert_eq!(residency_weighted_clock_mhz(&r, &table), Some(0));
    }

    #[test]
    fn clock_is_unknown_when_no_counter_advanced() {
        // Distinct from the parked case above: there, ticks accumulated in
        // `OFF`. Here nothing moved at all, which says nothing about the
        // clock — the first tick after startup looks exactly like this.
        let table = m3_table();
        assert_eq!(residency_weighted_clock_mhz(&[], &table), None);
        let r = residency(&[("OFF", 0), ("P1", 0)]);
        assert_eq!(residency_weighted_clock_mhz(&r, &table), None);
    }

    #[test]
    fn clock_is_unknown_when_the_table_cannot_name_a_state_that_ran() {
        // A chip that spent time in a state the decoded table has no entry
        // for means the two sources disagree. Reporting a clock derived from a
        // mismatched table would be a confident wrong answer.
        let table = m3_table();
        let r = residency(&[("P1", 10), ("P99", 10)]);
        assert_eq!(residency_weighted_clock_mhz(&r, &table), None);
        assert_eq!(active_residency_pct(&r, &table), None);
    }

    #[test]
    fn clock_ignores_padding_states_that_never_ran() {
        // The channel reports a fixed-size state array: an M3 answers with
        // P1..P15 against a 14-entry DVFS table, the surplus permanently at
        // zero. Treating that as a disagreement would drop the CLK column on
        // every Apple Silicon Mac — which is exactly what it did on the first
        // run against real hardware.
        let table = m3_table();
        let r = residency(&[("OFF", 900), ("P1", 100), ("P14", 0), ("P15", 0)]);
        assert_eq!(residency_weighted_clock_mhz(&r, &table), Some(338));

        let pct = active_residency_pct(&r, &table).expect("counters advanced");
        assert!((pct - 10.0).abs() < 0.001, "expected 10%, got {pct}");
    }

    #[test]
    fn clock_survives_a_full_second_of_maximum_residency() {
        // 24 MHz tick counters over a long uptime overflow `u64` maths if the
        // weighting is done in 64 bits; the accumulator is `u128` for that
        // reason and this pins it.
        let table = m3_table();
        let r = residency(&[("P13", u64::MAX)]);
        assert_eq!(residency_weighted_clock_mhz(&r, &table), Some(1338));
    }

    // ---- active residency -------------------------------------------------

    #[test]
    fn active_residency_is_the_unparked_fraction() {
        let table = m3_table();
        let r = residency(&[("OFF", 750), ("P1", 250)]);
        let pct = active_residency_pct(&r, &table).expect("counters advanced");
        assert!((pct - 25.0).abs() < 0.001, "expected 25%, got {pct}");
    }

    #[test]
    fn active_residency_is_unknown_when_nothing_advanced() {
        assert_eq!(active_residency_pct(&[], &m3_table()), None);
    }

    // ---- energy -----------------------------------------------------------

    #[test]
    fn energy_converts_every_unit_the_channels_use() {
        let one_second = Duration::from_secs(1);
        // The same GPU reports 159 324 641 nJ on one channel and 153 mJ on
        // another over the same second — 0.159 W and 0.153 W. Both readings
        // have to land in watts.
        let nano = energy_delta_to_watts(159_324_641, "nJ", one_second).unwrap();
        let milli = energy_delta_to_watts(153, "mJ", one_second).unwrap();
        assert!((nano - 0.159).abs() < 0.001, "nJ path: {nano}");
        assert!((milli - 0.153).abs() < 0.001, "mJ path: {milli}");
    }

    #[test]
    fn energy_accepts_both_spellings_of_micro() {
        let one_second = Duration::from_secs(1);
        for unit in ["uJ", "µJ", "μJ"] {
            let w = energy_delta_to_watts(1_000_000, unit, one_second)
                .unwrap_or_else(|| panic!("unit {unit} must be recognised"));
            assert!((w - 1.0).abs() < 0.001, "unit {unit}: {w}");
        }
    }

    #[test]
    fn energy_scales_by_the_real_interval() {
        // The collector runs at 1 Hz but a loaded machine can deliver a tick
        // late. Dividing by a hard-coded second would then overstate power.
        let w = energy_delta_to_watts(2_000, "mJ", Duration::from_secs(2)).unwrap();
        assert!((w - 1.0).abs() < 0.001, "got {w}");
    }

    #[test]
    fn energy_rejects_an_unknown_unit() {
        assert_eq!(
            energy_delta_to_watts(1, "furlongs", Duration::from_secs(1)),
            None
        );
    }

    #[test]
    fn energy_rejects_a_counter_reset() {
        // A negative delta means the counter wrapped or the subscription was
        // rebuilt. There is no average power to report for that interval.
        assert_eq!(
            energy_delta_to_watts(-5, "mJ", Duration::from_secs(1)),
            None
        );
    }

    #[test]
    fn energy_rejects_a_zero_interval() {
        assert_eq!(energy_delta_to_watts(100, "mJ", Duration::ZERO), None);
    }

    // ---- percentage clamp -------------------------------------------------

    #[test]
    fn driver_percentages_are_clamped_to_the_gauge_range() {
        assert_eq!(clamp_pct(-1), 0.0);
        assert_eq!(clamp_pct(0), 0.0);
        assert_eq!(clamp_pct(62), 62.0);
        assert_eq!(clamp_pct(101), 100.0);
    }
}
