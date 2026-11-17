//! Measured speed and memory. Numbers come from the engine's own timings and
//! are cross-checked against the wall clock seen by the daemon.

use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::VecDeque;

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct Sample {
    /// Time to first token, measured by the daemon (request sent to first token received).
    pub ttft_ms: f64,
    pub prompt_tokens: u64,
    pub prompt_tps: f64,
    pub gen_tokens: u64,
    pub gen_tps: f64,
    /// Generation speed from the wall clock: tokens after the first over the
    /// time from first to last token. Close to `gen_tps` when the engine's numbers are honest.
    pub wall_gen_tps: f64,
    pub at: u64,
}

/// Engine-reported timings in a common shape.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Timings {
    pub prompt_tokens: u64,
    pub prompt_ms: f64,
    pub gen_tokens: u64,
    pub gen_ms: f64,
}

impl Timings {
    /// llama-server's `timings` object.
    pub fn from_llama(t: &Value) -> Option<Timings> {
        Some(Timings {
            prompt_tokens: t.get("prompt_n")?.as_u64()?,
            prompt_ms: t.get("prompt_ms")?.as_f64()?,
            gen_tokens: t.get("predicted_n")?.as_u64()?,
            gen_ms: t.get("predicted_ms")?.as_f64()?,
        })
    }

    /// Ollama's final message (durations in nanoseconds).
    pub fn from_ollama(v: &Value) -> Option<Timings> {
        Some(Timings {
            prompt_tokens: v
                .get("prompt_eval_count")
                .and_then(Value::as_u64)
                .unwrap_or(0),
            prompt_ms: v
                .get("prompt_eval_duration")
                .and_then(Value::as_f64)
                .unwrap_or(0.0)
                / 1e6,
            gen_tokens: v.get("eval_count")?.as_u64()?,
            gen_ms: v.get("eval_duration")?.as_f64()? / 1e6,
        })
    }

    pub fn sample(&self, ttft_ms: f64, first_to_last_ms: f64, at: u64) -> Sample {
        let rate = |n: u64, ms: f64| {
            if ms > 0.0 {
                n as f64 * 1000.0 / ms
            } else {
                0.0
            }
        };
        Sample {
            ttft_ms,
            prompt_tokens: self.prompt_tokens,
            prompt_tps: rate(self.prompt_tokens, self.prompt_ms),
            gen_tokens: self.gen_tokens,
            gen_tps: rate(self.gen_tokens, self.gen_ms),
            wall_gen_tps: rate(self.gen_tokens.saturating_sub(1), first_to_last_ms),
            at,
        }
    }
}

/// Last sample plus a rolling window per model.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Rolling {
    pub last: Option<Sample>,
    #[serde(skip)]
    window: VecDeque<Sample>,
    pub count: u64,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct Averages {
    pub ttft_ms: f64,
    pub prompt_tps: f64,
    pub gen_tps: f64,
    pub samples: usize,
}

impl Rolling {
    pub const WINDOW: usize = 20;

    pub fn push(&mut self, s: Sample) {
        if self.window.len() == Self::WINDOW {
            self.window.pop_front();
        }
        self.window.push_back(s.clone());
        self.last = Some(s);
        self.count += 1;
    }

    pub fn averages(&self) -> Averages {
        let n = self.window.len();
        if n == 0 {
            return Averages::default();
        }
        let avg = |f: fn(&Sample) -> f64| self.window.iter().map(f).sum::<f64>() / n as f64;
        Averages {
            ttft_ms: avg(|s| s.ttft_ms),
            prompt_tps: avg(|s| s.prompt_tps),
            gen_tps: avg(|s| s.gen_tps),
            samples: n,
        }
    }
}

/// Resident memory of a process in bytes.
pub fn resident_bytes(pid: u32) -> Option<u64> {
    use sysinfo::{Pid, ProcessRefreshKind, ProcessesToUpdate, System};
    let mut sys = System::new().ok()?;
    let pid = Pid::from_u32(pid);
    sys.refresh_processes_specifics(
        ProcessesToUpdate::Some(&[pid]),
        true,
        ProcessRefreshKind::nothing().with_memory(),
    );
    sys.process(pid).map(|p| p.memory())
}

/// A process's executable name.
pub fn process_name(pid: u32) -> Option<String> {
    use sysinfo::{Pid, ProcessesToUpdate, System};
    let mut sys = System::new().ok()?;
    let pid = Pid::from_u32(pid);
    sys.refresh_processes(ProcessesToUpdate::Some(&[pid]), true);
    sys.process(pid)
        .map(|p| p.name().to_string_lossy().into_owned())
}

/// GPU memory a process holds, from `nvidia-smi` (NVIDIA only).
pub fn gpu_bytes(pid: u32) -> Option<u64> {
    let out = crate::hardware::run_probe(
        "nvidia-smi",
        &[
            "--query-compute-apps=pid,used_memory",
            "--format=csv,noheader,nounits",
        ],
    )?;
    parse_compute_apps(&out, pid)
}

pub fn parse_compute_apps(out: &str, pid: u32) -> Option<u64> {
    out.lines().find_map(|l| {
        let (p, m) = l.split_once(',')?;
        (p.trim().parse::<u32>().ok()? == pid)
            .then(|| m.trim().parse::<u64>().ok().map(|m| m * crate::util::MIB))?
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn llama_timings() {
        let t = Timings::from_llama(&json!({
            "prompt_n": 20, "prompt_ms": 100.0, "predicted_n": 51, "predicted_ms": 1000.0,
            "prompt_per_second": 200.0, "predicted_per_second": 51.0
        }))
        .unwrap();
        let s = t.sample(150.0, 1000.0, 1);
        assert_eq!(s.prompt_tps, 200.0);
        assert_eq!(s.gen_tps, 51.0);
        assert_eq!(s.wall_gen_tps, 50.0);
        assert_eq!(s.ttft_ms, 150.0);
    }

    #[test]
    fn ollama_timings() {
        let t = Timings::from_ollama(&json!({
            "done": true, "prompt_eval_count": 10, "prompt_eval_duration": 50_000_000u64,
            "eval_count": 30, "eval_duration": 1_500_000_000u64
        }))
        .unwrap();
        let s = t.sample(80.0, 1450.0, 1);
        assert_eq!(s.prompt_tps, 200.0);
        assert_eq!(s.gen_tps, 20.0);
        assert!((s.wall_gen_tps - 20.0).abs() < 0.01);
    }

    #[test]
    fn rolling_window() {
        let mut r = Rolling::default();
        for i in 0..25 {
            r.push(Sample {
                gen_tps: i as f64,
                ..Default::default()
            });
        }
        assert_eq!(r.count, 25);
        assert_eq!(r.last.as_ref().unwrap().gen_tps, 24.0);
        let a = r.averages();
        assert_eq!(a.samples, 20);
        assert_eq!(a.gen_tps, (5..25).sum::<i32>() as f64 / 20.0);
    }

    #[test]
    fn compute_apps_fixture() {
        let out = "4242, 1834\n99, 10\n";
        assert_eq!(parse_compute_apps(out, 4242), Some(1834 * crate::util::MIB));
        assert_eq!(parse_compute_apps(out, 7), None);
    }

    #[test]
    fn measures_own_memory() {
        assert!(resident_bytes(std::process::id()).unwrap() > 0);
    }
}
