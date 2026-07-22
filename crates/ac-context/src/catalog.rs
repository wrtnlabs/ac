//! Budgeted catalog rendering ([docs/ac-context.md] §6). A catalog scales with
//! what the user installed, not with the task; rendered unbounded it crowds
//! working context out of the window (R4). This renderer fits a catalog to a
//! character budget, degrading through a fixed lawful order and reporting every
//! loss — there is no silent step (I3, I4).

use std::fmt;

/// One catalog entry under a deterministic total rank (§6): provenance `tier`,
/// then `name`, then `locator`. The renderer sorts by that triple, so a given
/// entry set always renders in one order.
#[derive(Debug, Clone)]
pub struct CatalogEntry {
    /// Provenance tier — lower ranks first (e.g. bundled before user before wire).
    pub tier: u32,
    pub name: String,
    pub locator: String,
    /// Optional description; may be empty.
    pub description: String,
}

impl CatalogEntry {
    pub fn new(
        tier: u32,
        name: impl Into<String>,
        locator: impl Into<String>,
        description: impl Into<String>,
    ) -> Self {
        Self {
            tier,
            name: name.into(),
            locator: locator.into(),
            description: description.into(),
        }
    }
}

/// The lawful degradation order (§6). The renderer takes the *first* level whose
/// rendering fits the budget.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DegradationLevel {
    /// D0 — every entry, full (capped) descriptions.
    Full,
    /// D1 — all entries kept; description space above the minimums is allocated
    /// character-fairly, short descriptions donating to long ones.
    Redistributed,
    /// D2 — minimum lines only; no descriptions fit.
    MinimumsOnly,
    /// D3 — entries omitted in rank order, each kept iff its minimum line fits
    /// the budget remaining.
    Omitted,
}

impl fmt::Display for DegradationLevel {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let s = match self {
            DegradationLevel::Full => "D0 (full)",
            DegradationLevel::Redistributed => "D1 (redistributed descriptions)",
            DegradationLevel::MinimumsOnly => "D2 (descriptions dropped)",
            DegradationLevel::Omitted => "D3 (entries omitted)",
        };
        f.write_str(s)
    }
}

/// What a rendering did — enough for a host to warn precisely (I4).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CatalogReport {
    pub level: DegradationLevel,
    pub total: usize,
    pub included: usize,
    pub omitted: usize,
    /// Description characters present (after the per-entry cap) but not rendered.
    pub truncated_chars: usize,
}

/// A rendered catalog: the text, the report, and a host-visible warning present
/// whenever any content was lost — a level below D0 *or* a per-entry-cap
/// truncation at D0 (R4/I4).
#[derive(Debug, Clone)]
pub struct CatalogRender {
    pub text: String,
    pub report: CatalogReport,
    pub warning: Option<String>,
}

const SEP: &str = " — ";
const SEP_LEN: usize = 3; // ' ', '—', ' ' — three chars

/// Render `entries` to fit `budget` characters, capping each description at
/// `per_entry_cap` first so one pathological entry cannot monopolize the budget.
pub fn render_catalog(
    entries: &[CatalogEntry],
    budget: usize,
    per_entry_cap: usize,
) -> CatalogRender {
    let total = entries.len();
    if total == 0 {
        return CatalogRender {
            text: String::new(),
            report: CatalogReport {
                level: DegradationLevel::Full,
                total: 0,
                included: 0,
                omitted: 0,
                truncated_chars: 0,
            },
            warning: None,
        };
    }

    // Deterministic total rank: tier, then name, then locator.
    let mut sorted: Vec<&CatalogEntry> = entries.iter().collect();
    sorted.sort_by(|a, b| {
        a.tier
            .cmp(&b.tier)
            .then_with(|| a.name.cmp(&b.name))
            .then_with(|| a.locator.cmp(&b.locator))
    });

    // Per-entry cap first (§6): normalize descriptions under a hard cap.
    let mut cap_dropped = 0usize;
    let capped: Vec<String> = sorted
        .iter()
        .map(|e| {
            let (d, dropped) = cap_chars(&e.description, per_entry_cap);
            cap_dropped += dropped;
            d
        })
        .collect();

    let min_lines: Vec<String> = sorted.iter().map(|e| min_line(e)).collect();
    let min_len: Vec<usize> = min_lines.iter().map(|l| l.chars().count()).collect();
    // Full description segment " — {desc}" per entry, and its char cost.
    let desc_want: Vec<usize> = capped
        .iter()
        .map(|d| {
            if d.is_empty() {
                0
            } else {
                SEP_LEN + d.chars().count()
            }
        })
        .collect();

    // ---- D0: every entry, full capped descriptions. ----
    let d0_cost = sum(&min_len) + sum(&desc_want) + newline_cost(total);
    if d0_cost <= budget {
        let text = join(
            (0..total)
                .map(|i| full_line(&min_lines[i], &capped[i]))
                .collect(),
        );
        return finish(text, DegradationLevel::Full, total, total, 0, cap_dropped);
    }

    let sum_min = sum(&min_len) + newline_cost(total);

    // ---- D3: minimums do not all fit → omit in rank order. ----
    if sum_min > budget {
        let mut running = 0usize;
        let mut lines = Vec::new();
        let mut included = 0usize;
        let mut shown_desc_dropped = 0usize;
        for i in 0..total {
            let add = min_len[i] + if included > 0 { 1 } else { 0 };
            if running + add <= budget {
                running += add;
                lines.push(min_lines[i].clone());
                included += 1;
                // This entry's description is not rendered at D3.
                shown_desc_dropped += capped[i].chars().count();
            }
            // Continue past a miss (I3): a later, shorter minimum may still fit.
        }
        let omitted = total - included;
        return finish(
            join(lines),
            DegradationLevel::Omitted,
            total,
            included,
            omitted,
            cap_dropped + shown_desc_dropped,
        );
    }

    // ---- D1 / D2: all minimums fit; share the surplus across descriptions. ----
    let surplus = budget - sum_min;
    let alloc = water_fill(&desc_want, surplus);

    let mut lines = Vec::with_capacity(total);
    let mut shown_desc_dropped = 0usize;
    let mut any_description = false;
    for i in 0..total {
        let available = alloc[i].saturating_sub(SEP_LEN);
        if desc_want[i] == 0 || available == 0 {
            // No room (or no description) for this entry's segment.
            shown_desc_dropped += capped[i].chars().count();
            lines.push(min_lines[i].clone());
            continue;
        }
        let (shown, dropped) = cap_chars(&capped[i], available);
        shown_desc_dropped += dropped;
        if shown.is_empty() {
            lines.push(min_lines[i].clone());
        } else {
            any_description = true;
            lines.push(format!("{}{SEP}{shown}", min_lines[i]));
        }
    }

    let level = if any_description {
        DegradationLevel::Redistributed
    } else {
        DegradationLevel::MinimumsOnly
    };
    let truncated = cap_dropped + shown_desc_dropped;
    finish(join(lines), level, total, total, 0, truncated)
}

fn finish(
    text: String,
    level: DegradationLevel,
    total: usize,
    included: usize,
    omitted: usize,
    truncated_chars: usize,
) -> CatalogRender {
    // A warning fires on *any* loss, not just a sub-D0 level (I4): the per-entry
    // cap can drop description characters while the level is still D0, and a host
    // keying off `warning.is_some()` must not miss that.
    let warning = if truncated_chars > 0 || omitted > 0 {
        Some(format!(
            "catalog rendered at {level}: {included}/{total} entries shown, {truncated_chars} description characters omitted"
        ))
    } else {
        None
    };
    CatalogRender {
        text,
        report: CatalogReport {
            level,
            total,
            included,
            omitted,
            truncated_chars,
        },
        warning,
    }
}

fn min_line(e: &CatalogEntry) -> String {
    format!("- {} ({})", e.name, e.locator)
}

fn full_line(min: &str, desc: &str) -> String {
    if desc.is_empty() {
        min.to_string()
    } else {
        format!("{min}{SEP}{desc}")
    }
}

fn join(lines: Vec<String>) -> String {
    lines.join("\n")
}

fn newline_cost(n: usize) -> usize {
    n.saturating_sub(1)
}

fn sum(xs: &[usize]) -> usize {
    xs.iter().sum()
}

/// Truncate `s` to `n` characters, ellipsis-terminated if anything was dropped.
/// Returns the string and the number of original characters removed.
fn cap_chars(s: &str, n: usize) -> (String, usize) {
    let chars: Vec<char> = s.chars().collect();
    let len = chars.len();
    if len <= n {
        return (s.to_string(), 0);
    }
    if n == 0 {
        return (String::new(), len);
    }
    let keep = n - 1; // room for the ellipsis
    let head: String = chars.iter().take(keep).collect();
    (format!("{head}…"), len - keep)
}

/// Allocate `surplus` characters across `wants` fairly: each gets at most its
/// want and at most an equal share of what remains, so short wants donate their
/// unused share to long ones (§6). Sum of the allocation never exceeds surplus.
fn water_fill(wants: &[usize], surplus: usize) -> Vec<usize> {
    let n = wants.len();
    let mut alloc = vec![0usize; n];
    let mut order: Vec<usize> = (0..n).collect();
    order.sort_by_key(|&i| wants[i]);
    let mut remaining = surplus;
    let mut left = n;
    for &i in &order {
        if left == 0 {
            break;
        }
        let share = remaining / left;
        let a = wants[i].min(share);
        alloc[i] = a;
        remaining -= a;
        left -= 1;
    }
    alloc
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(tier: u32, name: &str, desc: &str) -> CatalogEntry {
        CatalogEntry::new(tier, name, format!("/skills/{name}/SKILL.md"), desc)
    }

    #[test]
    fn d0_renders_everything_when_it_fits() {
        let entries = vec![
            entry(0, "alpha", "does alpha"),
            entry(0, "beta", "does beta"),
        ];
        let r = render_catalog(&entries, 10_000, 200);
        assert_eq!(r.report.level, DegradationLevel::Full);
        assert_eq!(r.report.included, 2);
        assert!(r.warning.is_none());
        assert!(r.text.contains("does alpha") && r.text.contains("does beta"));
        // Deterministic rank: alpha before beta.
        assert!(r.text.find("alpha").unwrap() < r.text.find("beta").unwrap());
    }

    #[test]
    fn per_entry_cap_bounds_one_pathological_description() {
        let entries = vec![entry(0, "x", &"D".repeat(500))];
        let r = render_catalog(&entries, 10_000, 20);
        assert_eq!(r.report.level, DegradationLevel::Full);
        assert!(r.report.truncated_chars > 0, "the cap dropped chars (I4)");
        assert!(r.text.contains("…"));
        // I4: even at D0, a truncation must surface a warning — a host keying off
        // `warning.is_some()` must not miss a per-entry-cap drop.
        assert!(
            r.warning.is_some(),
            "a D0 rendering that truncated descriptions must still warn"
        );
    }

    #[test]
    fn d2_drops_descriptions_when_only_minimums_fit() {
        // Budget fits both minimum lines but leaves no room for descriptions.
        let entries = vec![
            entry(0, "alpha", "some description"),
            entry(0, "beta", "another"),
        ];
        let min_only =
            min_line(&entries[0]).chars().count() + 1 + min_line(&entries[1]).chars().count();
        let r = render_catalog(&entries, min_only, 200);
        assert_eq!(r.report.level, DegradationLevel::MinimumsOnly);
        assert_eq!(r.report.included, 2);
        assert!(!r.text.contains("some description"));
        assert!(r.warning.is_some());
    }

    #[test]
    fn d1_redistributes_short_donors_to_long_descriptions() {
        // One tiny description, one long: with limited surplus the long one
        // should still get some room the short one didn't need.
        let entries = vec![entry(0, "aa", "x"), entry(0, "bb", &"L".repeat(80))];
        let min_cost =
            min_line(&entries[0]).chars().count() + 1 + min_line(&entries[1]).chars().count();
        let r = render_catalog(&entries, min_cost + 40, 200);
        assert_eq!(r.report.level, DegradationLevel::Redistributed);
        assert!(
            r.text.contains('L'),
            "the long description got redistributed room"
        );
    }

    #[test]
    fn d3_omits_entries_but_continues_past_a_miss() {
        // A huge minimum line ranked first must not blank the small ones after it.
        let big = CatalogEntry::new(0, "N".repeat(300), "/x", "");
        let small = entry(1, "small", "");
        let entries = vec![big, small];
        let r = render_catalog(&entries, 40, 200);
        assert_eq!(r.report.level, DegradationLevel::Omitted);
        assert!(r.report.omitted >= 1);
        assert!(
            r.text.contains("small"),
            "the walk continued past the oversized entry (I3)"
        );
    }

    #[test]
    fn empty_catalog_renders_nothing_at_d0() {
        let r = render_catalog(&[], 100, 50);
        assert_eq!(r.report.level, DegradationLevel::Full);
        assert_eq!(r.report.total, 0);
        assert!(r.text.is_empty());
        assert!(r.warning.is_none());
    }

    fn level_rank(l: DegradationLevel) -> u8 {
        match l {
            DegradationLevel::Full => 0,
            DegradationLevel::Redistributed => 1,
            DegradationLevel::MinimumsOnly => 2,
            DegradationLevel::Omitted => 3,
        }
    }

    #[test]
    fn degradation_level_improves_monotonically_with_budget() {
        // The level is decided by budget vs. the fixed thresholds d0_cost and
        // sum_min, so a larger budget never renders at a *worse* level.
        let entries = vec![
            entry(0, "alpha", &"A".repeat(60)),
            entry(1, "beta", &"B".repeat(60)),
            entry(2, "gamma", &"C".repeat(60)),
        ];
        let mut prev = level_rank(DegradationLevel::Omitted);
        for budget in [10usize, 30, 60, 120, 240, 480, 10_000] {
            let rank = level_rank(render_catalog(&entries, budget, 200).report.level);
            assert!(rank <= prev, "level must not regress as budget grows");
            prev = rank;
        }
    }

    #[test]
    fn d3_honors_rank_priority_even_against_entry_count() {
        // Strict rank priority (§6 D3): once a large high-ranked entry becomes
        // admissible, it is included even if it displaces several smaller
        // lower-ranked entries a tighter budget had shown. This is the one point
        // where the rank-order rule takes precedence over I3's monotonicity
        // SHOULD — asserted here so the behavior is pinned, not accidental.
        let big = CatalogEntry::new(0, "B".repeat(23), "/b", ""); // min line = 30 chars
        let smalls: Vec<CatalogEntry> = (0..4)
            .map(|i| CatalogEntry::new(1, format!("{i}"), "/s", "")) // min line = 8 chars each
            .collect();
        let mut entries = vec![big];
        entries.extend(smalls);

        // Tight budget: the big entry (30) does not fit; three smalls do (8+9+9=26).
        let tight = render_catalog(&entries, 26, 200);
        assert!(
            !tight.text.contains("BBB"),
            "big entry omitted when it doesn't fit"
        );
        assert!(tight.report.included >= 3, "smaller entries shown instead");

        // Larger budget: the big entry (30) now fits and takes priority,
        // displacing the smalls — fewer entries, but rank order honored.
        let loose = render_catalog(&entries, 31, 200);
        assert!(
            loose.text.contains("BBB"),
            "the higher-ranked entry is now included"
        );
        assert!(
            loose.report.included < tight.report.included,
            "rank priority can reduce the entry count at the D3 boundary"
        );
    }
}
