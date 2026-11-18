//! Options and the planner: every way a model can run on a fleet, the link
//! ceiling that rules splits out before anyone measures them, and the order
//! measured results put the rest in. The ratchet checks recorded results
//! against the planner so a change that picks a slower option fails CI.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};

use crate::topology::LinkKind;

pub const RESULT_SCHEMA: &str = "fonsviva.topology-result.v1";

/// A newer result may fall to this share of the best earlier one before the
/// ratchet asks for a written reason.
pub const REGRESSION_FLOOR: f64 = 0.9;

/// One machine in a fleet, with the memory free for a model on it.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Machine {
    pub name: String,
    pub compute: String,
    pub free_bytes: u64,
}

/// A link two machines share, with its measured round trip.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FleetLink {
    pub between: [String; 2],
    pub kind: LinkKind,
    pub rtt_ms: f64,
    pub route: String,
}

#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
pub struct Fleet {
    pub machines: Vec<Machine>,
    pub links: Vec<FleetLink>,
}

/// Where a model runs: one machine, or a layer split over a link.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Placement {
    /// Sorted machine names.
    pub machines: Vec<String>,
    /// The link a split runs over; none for one machine.
    pub link: Option<LinkKind>,
}

impl Placement {
    pub fn single(machine: &str) -> Self {
        Placement {
            machines: vec![machine.to_string()],
            link: None,
        }
    }

    pub fn split(machines: &[&str], link: LinkKind) -> Self {
        let mut machines: Vec<String> = machines.iter().map(|m| m.to_string()).collect();
        machines.sort();
        Placement {
            machines,
            link: Some(link),
        }
    }

    pub fn is_split(&self) -> bool {
        self.machines.len() > 1
    }

    fn boundaries(&self) -> usize {
        self.machines.len().saturating_sub(1)
    }
}

impl std::fmt::Display for Placement {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self.link {
            None => write!(f, "{} alone", self.machines.join("+")),
            Some(link) => write!(f, "{} over {link:?}", self.machines.join("+")),
        }
    }
}

/// Memory a model needs beyond its weights: KV cache and buffers, about an
/// eighth of the weights.
pub fn needed_bytes(model_bytes: u64) -> u64 {
    model_bytes + model_bytes / 8
}

/// The decode ceiling a split's link sets before any compute: one round trip
/// per boundary per token.
pub fn ceiling_tps(rtt_ms: f64, boundaries: usize) -> f64 {
    if rtt_ms <= 0.0 || boundaries == 0 {
        return f64::INFINITY;
    }
    1000.0 / (rtt_ms * boundaries as f64)
}

/// Every placement for a model of `model_bytes` on `fleet`: each machine
/// that holds it alone, and each pair that holds it together over a link
/// they share (the fastest link of each kind).
pub fn placements(fleet: &Fleet, model_bytes: u64) -> Vec<(Placement, Option<f64>)> {
    let need = needed_bytes(model_bytes);
    let mut out = Vec::new();
    for machine in &fleet.machines {
        if machine.free_bytes >= need {
            out.push((Placement::single(&machine.name), None));
        }
    }
    let mut fastest: BTreeMap<Placement, f64> = BTreeMap::new();
    for link in &fleet.links {
        let [a, b] = &link.between;
        let (Some(ma), Some(mb)) = (machine(fleet, a), machine(fleet, b)) else {
            continue;
        };
        if a == b || ma.free_bytes.saturating_add(mb.free_bytes) < need {
            continue;
        }
        let placement = Placement::split(&[a, b], link.kind);
        let entry = fastest.entry(placement).or_insert(link.rtt_ms);
        *entry = entry.min(link.rtt_ms);
    }
    for (placement, rtt) in fastest {
        let ceiling = ceiling_tps(rtt, placement.boundaries());
        out.push((placement, Some(ceiling)));
    }
    out
}

fn machine<'a>(fleet: &'a Fleet, name: &str) -> Option<&'a Machine> {
    fleet.machines.iter().find(|m| m.name == name)
}

/// One measured decode rate for a placement.
#[derive(Debug, Clone, PartialEq)]
pub struct Measured {
    pub placement: Placement,
    pub decode_tps: f64,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Candidate {
    pub placement: Placement,
    /// The best measured decode, when there is one.
    pub decode_tps: Option<f64>,
    /// A split's link ceiling.
    pub ceiling_tps: Option<f64>,
    pub why: String,
}

#[derive(Debug, Clone, PartialEq, Default)]
pub struct Plan {
    /// Best first. Measured options come before unmeasured ones.
    pub ranked: Vec<Candidate>,
    /// Splits whose link ceiling is below a measured single-machine rate.
    pub dropped: Vec<Candidate>,
}

impl Plan {
    pub fn choice(&self) -> Option<&Candidate> {
        self.ranked.first()
    }
}

/// Plans a model on a fleet. A placement that was measured is always a
/// candidate, since it ran. A split is dropped when its link ceiling is below
/// the best measured single machine. The rest are ordered by measured decode,
/// a tie going to fewer machines;
/// unmeasured ones follow, one machine first (most free memory first), then
/// splits by ceiling.
pub fn plan(fleet: &Fleet, model_bytes: u64, measured: &[Measured]) -> Plan {
    let mut best: BTreeMap<Placement, f64> = BTreeMap::new();
    for m in measured {
        let entry = best.entry(m.placement.clone()).or_insert(m.decode_tps);
        *entry = entry.max(m.decode_tps);
    }
    let mut candidates: BTreeMap<Placement, Option<f64>> =
        placements(fleet, model_bytes).into_iter().collect();
    for placement in best.keys() {
        candidates.entry(placement.clone()).or_insert_with(|| {
            placement
                .link
                .and_then(|kind| link_rtt(fleet, placement, kind))
                .map(|rtt| ceiling_tps(rtt, placement.boundaries()))
        });
    }
    let best_single = best
        .iter()
        .filter(|(p, _)| !p.is_split())
        .map(|(_, &tps)| tps)
        .fold(None, |acc: Option<f64>, tps| {
            Some(acc.map_or(tps, |a| a.max(tps)))
        });
    let mut ranked = Vec::new();
    let mut dropped = Vec::new();
    for (placement, ceiling) in candidates {
        let decode = best.get(&placement).copied();
        let below = match (ceiling, best_single, decode) {
            (Some(ceiling), Some(single), None) => ceiling < single,
            _ => false,
        };
        let why = match (decode, ceiling) {
            _ if below => format!(
                "the link caps decode at {:.0} tok/s, below {:.1} tok/s measured on one machine",
                ceiling.unwrap_or_default(),
                best_single.unwrap_or_default()
            ),
            (Some(tps), _) => format!("measured {tps:.1} tok/s decode"),
            (None, Some(ceiling)) => {
                format!("not measured; the link caps decode at {ceiling:.0} tok/s")
            }
            (None, None) => "not measured".to_string(),
        };
        let option = Candidate {
            placement,
            decode_tps: decode,
            ceiling_tps: ceiling,
            why,
        };
        if below {
            dropped.push(option);
        } else {
            ranked.push(option);
        }
    }
    let free = |p: &Placement| {
        p.machines
            .iter()
            .filter_map(|name| machine(fleet, name))
            .map(|m| m.free_bytes)
            .sum::<u64>()
    };
    ranked.sort_by(|a, b| match (a.decode_tps, b.decode_tps) {
        (Some(x), Some(y)) => y
            .total_cmp(&x)
            .then_with(|| a.placement.machines.len().cmp(&b.placement.machines.len()))
            .then_with(|| a.placement.cmp(&b.placement)),
        (Some(_), None) => std::cmp::Ordering::Less,
        (None, Some(_)) => std::cmp::Ordering::Greater,
        (None, None) => a
            .placement
            .is_split()
            .cmp(&b.placement.is_split())
            .then_with(|| {
                let (ca, cb) = (
                    a.ceiling_tps.unwrap_or(f64::INFINITY),
                    b.ceiling_tps.unwrap_or(f64::INFINITY),
                );
                cb.total_cmp(&ca)
            })
            .then_with(|| free(&b.placement).cmp(&free(&a.placement)))
            .then_with(|| a.placement.cmp(&b.placement)),
    });
    Plan { ranked, dropped }
}

fn link_rtt(fleet: &Fleet, placement: &Placement, kind: LinkKind) -> Option<f64> {
    fleet
        .links
        .iter()
        .filter(|l| l.kind == kind)
        .filter(|l| {
            let mut between = l.between.to_vec();
            between.sort();
            between == placement.machines
        })
        .map(|l| l.rtt_ms)
        .reduce(f64::min)
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Engine {
    pub name: String,
    pub version: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Model {
    pub id: String,
    pub commit: String,
    pub quant: String,
    pub bytes: u64,
}

/// One bench result: a model on a placement in a fleet, with the fleet as it
/// was measured.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BenchResult {
    pub schema: String,
    pub fleet: String,
    pub measured_at: String,
    pub machines: Vec<Machine>,
    pub links: Vec<FleetLink>,
    pub engine: Engine,
    pub model: Model,
    pub placement: Placement,
    /// True when this placement was the plan the bench ran as the answer.
    pub planned: bool,
    pub prompt_tps: f64,
    pub decode_tps: f64,
    pub log_sha256: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub regression_note: Option<String>,
}

/// Reads every topology result in `dir`. Files with another schema (probes,
/// channel runs) are skipped; a file that claims this schema must parse.
pub fn load_results(dir: &Path) -> Result<Vec<(PathBuf, BenchResult)>> {
    let mut out = Vec::new();
    let entries = std::fs::read_dir(dir).with_context(|| format!("reading {}", dir.display()))?;
    let mut paths: Vec<PathBuf> = entries
        .filter_map(|e| e.ok().map(|e| e.path()))
        .filter(|p| p.extension().is_some_and(|e| e == "json"))
        .collect();
    paths.sort();
    for path in paths {
        let text = std::fs::read_to_string(&path)?;
        let value: serde_json::Value =
            serde_json::from_str(&text).with_context(|| format!("{}", path.display()))?;
        match value.get("schema").and_then(|s| s.as_str()) {
            Some(RESULT_SCHEMA) => {}
            Some(other) if other.starts_with("fonsviva.topology") => {
                bail!("{}: unknown schema {other}", path.display())
            }
            _ => continue,
        }
        let result: BenchResult =
            serde_json::from_value(value).with_context(|| format!("{}", path.display()))?;
        out.push((path, result));
    }
    Ok(out)
}

type Group<'a> = BTreeMap<(String, String, String), Vec<&'a BenchResult>>;

fn by_model(results: &[BenchResult]) -> Group<'_> {
    let mut groups: Group = BTreeMap::new();
    for r in results {
        groups
            .entry((r.fleet.clone(), r.model.id.clone(), r.model.quant.clone()))
            .or_default()
            .push(r);
    }
    groups
}

fn schema_problems(r: &BenchResult) -> Vec<String> {
    let mut out = Vec::new();
    let name = format!(
        "{} {} {} {}",
        r.fleet, r.model.id, r.model.quant, r.placement
    );
    let mut problem = |ok: bool, what: &str| {
        if !ok {
            out.push(format!("{name}: {what}"));
        }
    };
    problem(!r.fleet.is_empty(), "no fleet name");
    problem(
        utc_stamp(&r.measured_at),
        "measured_at is not a UTC time like 2026-11-20T10:00:00Z",
    );
    problem(!r.machines.is_empty(), "no machines");
    problem(
        r.placement
            .machines
            .iter()
            .all(|m| r.machines.iter().any(|x| &x.name == m)),
        "the placement names a machine the result does not describe",
    );
    problem(
        r.placement.is_split() == r.placement.link.is_some(),
        "a split needs a link and one machine needs none",
    );
    problem(
        r.placement
            .link
            .is_none_or(|kind| link_rtt(&fleet_of(r), &r.placement, kind).is_some()),
        "the split's link is not among the measured links",
    );
    problem(
        r.links.iter().all(|l| l.rtt_ms > 0.0),
        "a link has no measured round trip",
    );
    problem(!r.engine.version.is_empty(), "no engine version");
    problem(!r.model.commit.is_empty(), "no model commit");
    problem(r.model.bytes > 0, "no model size");
    problem(r.decode_tps > 0.0 && r.prompt_tps > 0.0, "no tok/s");
    problem(
        r.log_sha256.len() == 64 && r.log_sha256.bytes().all(|b| b.is_ascii_hexdigit()),
        "log_sha256 is not a SHA-256",
    );
    out
}

fn utc_stamp(s: &str) -> bool {
    let b = s.as_bytes();
    b.len() == 20
        && b.iter().enumerate().all(|(i, &c)| match i {
            4 | 7 => c == b'-',
            10 => c == b'T',
            13 | 16 => c == b':',
            19 => c == b'Z',
            _ => c.is_ascii_digit(),
        })
}

fn fleet_of(r: &BenchResult) -> Fleet {
    Fleet {
        machines: r.machines.clone(),
        links: r.links.clone(),
    }
}

/// Every way `results` break the ratchet. Empty means it holds.
pub fn ratchet(results: &[BenchResult]) -> Vec<String> {
    let mut out: Vec<String> = results.iter().flat_map(schema_problems).collect();
    for ((fleet, model, quant), group) in by_model(results) {
        let name = format!("{fleet} {model} {quant}");
        let newest = group
            .iter()
            .max_by(|a, b| a.measured_at.cmp(&b.measured_at))
            .expect("groups are never empty");
        let mut current: BTreeMap<&Placement, &BenchResult> = BTreeMap::new();
        for &r in &group {
            match current.get(&r.placement) {
                Some(seen) if seen.measured_at == r.measured_at => out.push(format!(
                    "{name}: two {} results share measured_at {}",
                    r.placement, r.measured_at
                )),
                Some(seen) if seen.measured_at > r.measured_at => {}
                _ => {
                    current.insert(&r.placement, r);
                }
            }
        }
        let current: Vec<&BenchResult> = current.into_values().collect();
        let measured: Vec<Measured> = current
            .iter()
            .map(|r| Measured {
                placement: r.placement.clone(),
                decode_tps: r.decode_tps,
            })
            .collect();
        let winner = current
            .iter()
            .max_by(|a, b| a.decode_tps.total_cmp(&b.decode_tps))
            .expect("groups are never empty");
        let plan = plan(&fleet_of(newest), newest.model.bytes, &measured);
        match plan.choice() {
            Some(choice) if choice.decode_tps == Some(winner.decode_tps) => {}
            Some(choice) => out.push(format!(
                "{name}: the planner picks {} but {} measured faster ({:.1} tok/s)",
                choice.placement, winner.placement, winner.decode_tps
            )),
            None => out.push(format!("{name}: the planner offers nothing")),
        }
        for r in current.iter().filter(|r| r.planned) {
            if r.placement != winner.placement && r.decode_tps < winner.decode_tps {
                out.push(format!(
                    "{name}: {} was recorded as the plan at {:.1} tok/s, but {} measured {:.1}",
                    r.placement, r.decode_tps, winner.placement, winner.decode_tps
                ));
            }
        }
        let fits_alone = placements(&fleet_of(newest), newest.model.bytes)
            .iter()
            .any(|(p, _)| !p.is_split())
            || group.iter().any(|r| !r.placement.is_split());
        let best_single = current
            .iter()
            .filter(|r| !r.placement.is_split())
            .map(|r| r.decode_tps)
            .reduce(f64::max);
        for r in current
            .iter()
            .filter(|r| r.planned && r.placement.is_split())
        {
            match best_single {
                Some(single) if r.decode_tps <= single => out.push(format!(
                    "{name}: the split {} was planned at {:.1} tok/s but one machine measured {single:.1}",
                    r.placement, r.decode_tps
                )),
                None if fits_alone => out.push(format!(
                    "{name}: the split {} was planned though the model fits one machine and no single-machine result exists to beat",
                    r.placement
                )),
                _ => {}
            }
        }
        let mut series: BTreeMap<(String, String, Placement), Vec<&BenchResult>> = BTreeMap::new();
        for r in &group {
            series
                .entry((
                    r.engine.name.clone(),
                    r.engine.version.clone(),
                    r.placement.clone(),
                ))
                .or_default()
                .push(r);
        }
        for ((engine, version, placement), mut runs) in series {
            runs.sort_by(|a, b| a.measured_at.cmp(&b.measured_at));
            let Some((last, earlier)) = runs.split_last() else {
                continue;
            };
            let Some(best) = earlier.iter().map(|r| r.decode_tps).reduce(f64::max) else {
                continue;
            };
            if last.decode_tps < best * REGRESSION_FLOOR && last.regression_note.is_none() {
                out.push(format!(
                    "{name} {placement} on {engine} {version}: {:.1} tok/s at {} is below 90% of the best earlier {best:.1}; add a regression_note",
                    last.decode_tps, last.measured_at
                ));
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    const GB: u64 = 1_000_000_000;

    fn fleet() -> Fleet {
        Fleet {
            machines: vec![
                Machine {
                    name: "mac".into(),
                    compute: "Apple M5, Metal".into(),
                    free_bytes: 24 * GB,
                },
                Machine {
                    name: "parish1".into(),
                    compute: "RTX 4080 Laptop".into(),
                    free_bytes: 10 * GB,
                },
            ],
            links: vec![
                FleetLink {
                    between: ["mac".into(), "parish1".into()],
                    kind: LinkKind::Cable,
                    rtt_ms: 0.26,
                    route: "bridge0 pinned".into(),
                },
                FleetLink {
                    between: ["mac".into(), "parish1".into()],
                    kind: LinkKind::Tailnet,
                    rtt_ms: 14.3,
                    route: "utun14".into(),
                },
            ],
        }
    }

    #[test]
    fn the_link_ceiling_is_one_round_trip_per_boundary_per_token() {
        assert!((ceiling_tps(0.26, 1) - 3846.0).abs() < 1.0);
        assert!((ceiling_tps(14.3, 1) - 69.9).abs() < 0.1);
        assert!((ceiling_tps(10.0, 2) - 50.0).abs() < 1e-9);
    }

    #[test]
    fn placements_list_each_machine_that_fits_and_each_pair_over_each_link() {
        let all = placements(&fleet(), 8 * GB);
        let names: Vec<String> = all.iter().map(|(p, _)| p.to_string()).collect();
        assert_eq!(
            names,
            [
                "mac alone",
                "parish1 alone",
                "mac+parish1 over Cable",
                "mac+parish1 over Tailnet"
            ]
        );
        let big = placements(&fleet(), 28 * GB);
        assert!(big.iter().all(|(p, _)| p.is_split()));
    }

    #[test]
    fn a_split_whose_ceiling_is_below_a_measured_machine_is_dropped_unmeasured() {
        let plan = plan(
            &fleet(),
            8 * GB,
            &[Measured {
                placement: Placement::single("mac"),
                decode_tps: 80.0,
            }],
        );
        assert_eq!(plan.choice().unwrap().placement, Placement::single("mac"));
        let dropped: Vec<String> = plan
            .dropped
            .iter()
            .map(|o| o.placement.to_string())
            .collect();
        assert_eq!(dropped, ["mac+parish1 over Tailnet"]);
        let ranked: Vec<String> = plan
            .ranked
            .iter()
            .map(|o| o.placement.to_string())
            .collect();
        assert_eq!(
            ranked,
            ["mac alone", "parish1 alone", "mac+parish1 over Cable"]
        );
        assert_eq!(
            plan.ranked[2].why,
            "not measured; the link caps decode at 3846 tok/s"
        );
    }

    #[test]
    fn a_measured_split_that_wins_is_chosen_and_an_unmeasured_one_never_outranks_a_measured_one() {
        let plan_ = plan(
            &fleet(),
            8 * GB,
            &[
                Measured {
                    placement: Placement::single("mac"),
                    decode_tps: 40.0,
                },
                Measured {
                    placement: Placement::split(&["mac", "parish1"], LinkKind::Cable),
                    decode_tps: 55.0,
                },
            ],
        );
        assert_eq!(
            plan_.choice().unwrap().placement,
            Placement::split(&["parish1", "mac"], LinkKind::Cable)
        );
        assert!(plan_.ranked.iter().skip(2).all(|o| o.decode_tps.is_none()));
        let unmeasured = plan(&fleet(), 8 * GB, &[]);
        assert_eq!(
            unmeasured.choice().unwrap().placement,
            Placement::single("mac")
        );
        assert!(unmeasured.dropped.is_empty());
    }

    #[test]
    fn a_model_no_machine_holds_runs_split_with_the_fastest_link_first() {
        let plan = plan(&fleet(), 28 * GB, &[]);
        let names: Vec<String> = plan
            .ranked
            .iter()
            .map(|o| o.placement.to_string())
            .collect();
        assert_eq!(
            names,
            ["mac+parish1 over Cable", "mac+parish1 over Tailnet"]
        );
    }
}
