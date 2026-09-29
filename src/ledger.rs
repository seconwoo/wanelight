//! Wear ledger: per-cell accumulated light output for one monitor.
//!
//! Each cell stores "white-seconds": brightness (1.0 = SDR white) times the
//! seconds it was shown, after Wanelight's dimming. A parallel grid stores the
//! white-seconds that dimming avoided. Files live in the data directory as
//! `wear-<monitor id>.bin`.

use std::path::PathBuf;

use crate::util;

const MAGIC: &[u8; 4] = b"WLL1";

#[derive(Clone, Debug)]
pub struct Ledger {
    pub name: String,
    pub gw: usize,
    pub gh: usize,
    /// Seconds the panel was on while tracked.
    pub seconds: f64,
    pub emitted: Vec<f32>,
    pub avoided: Vec<f32>,
}

impl Ledger {
    pub fn new(name: &str, gw: usize, gh: usize) -> Self {
        Self {
            name: name.to_string(),
            gw,
            gh,
            seconds: 0.0,
            emitted: vec![0.0; gw * gh],
            avoided: vec![0.0; gw * gh],
        }
    }

    pub fn path(monitor_id: &str) -> PathBuf {
        util::data_dir().join(format!("wear-{monitor_id}.bin"))
    }

    /// Loads the ledger for a monitor, resampling if the grid size changed.
    pub fn load_or_new(monitor_id: &str, name: &str, gw: usize, gh: usize) -> Self {
        match Self::load(&Self::path(monitor_id)) {
            Some(l) if l.gw == gw && l.gh == gh => Ledger { name: name.to_string(), ..l },
            Some(l) => {
                let mut n = Ledger::new(name, gw, gh);
                n.seconds = l.seconds;
                for y in 0..gh {
                    for x in 0..gw {
                        let sx = x * l.gw / gw;
                        let sy = y * l.gh / gh;
                        n.emitted[y * gw + x] = l.emitted[sy * l.gw + sx];
                        n.avoided[y * gw + x] = l.avoided[sy * l.gw + sx];
                    }
                }
                n
            }
            None => Ledger::new(name, gw, gh),
        }
    }

    pub fn load(path: &std::path::Path) -> Option<Self> {
        let data = std::fs::read(path).ok()?;
        let mut r = Reader { data: &data, pos: 0 };
        if r.take(4)? != MAGIC {
            return None;
        }
        let gw = r.u32()? as usize;
        let gh = r.u32()? as usize;
        let seconds = r.f64()?;
        let name_len = r.u32()? as usize;
        let name = String::from_utf8_lossy(r.take(name_len)?).into_owned();
        if gw == 0 || gh == 0 || gw * gh > 4_000_000 {
            return None;
        }
        let mut emitted = Vec::with_capacity(gw * gh);
        let mut avoided = Vec::with_capacity(gw * gh);
        for _ in 0..gw * gh {
            emitted.push(r.f32()?);
        }
        for _ in 0..gw * gh {
            avoided.push(r.f32()?);
        }
        Some(Ledger { name, gw, gh, seconds, emitted, avoided })
    }

    pub fn save(&self, monitor_id: &str) -> std::io::Result<()> {
        let mut out = Vec::with_capacity(32 + self.name.len() + self.emitted.len() * 8);
        out.extend_from_slice(MAGIC);
        out.extend_from_slice(&(self.gw as u32).to_le_bytes());
        out.extend_from_slice(&(self.gh as u32).to_le_bytes());
        out.extend_from_slice(&self.seconds.to_le_bytes());
        out.extend_from_slice(&(self.name.len() as u32).to_le_bytes());
        out.extend_from_slice(self.name.as_bytes());
        for v in self.emitted.iter().chain(self.avoided.iter()) {
            out.extend_from_slice(&v.to_le_bytes());
        }
        util::write_atomic(&Self::path(monitor_id), &out)
    }

    pub fn reset(&mut self) {
        self.seconds = 0.0;
        self.emitted.iter_mut().for_each(|v| *v = 0.0);
        self.avoided.iter_mut().for_each(|v| *v = 0.0);
    }

    /// Lists every ledger file in the data directory as (monitor id, ledger).
    pub fn load_all() -> Vec<(String, Ledger)> {
        let mut out = Vec::new();
        if let Ok(entries) = std::fs::read_dir(util::data_dir()) {
            for e in entries.flatten() {
                let name = e.file_name().to_string_lossy().into_owned();
                if let Some(id) = name.strip_prefix("wear-").and_then(|s| s.strip_suffix(".bin"))
                    && let Some(l) = Ledger::load(&e.path()) {
                        out.push((id.to_string(), l));
                    }
            }
        }
        out.sort_by(|a, b| a.1.name.cmp(&b.1.name).then(b.1.seconds.total_cmp(&a.1.seconds)));
        out
    }
}

struct Reader<'a> {
    data: &'a [u8],
    pos: usize,
}

impl<'a> Reader<'a> {
    fn take(&mut self, n: usize) -> Option<&'a [u8]> {
        let s = self.data.get(self.pos..self.pos + n)?;
        self.pos += n;
        Some(s)
    }
    fn u32(&mut self) -> Option<u32> {
        Some(u32::from_le_bytes(self.take(4)?.try_into().ok()?))
    }
    fn f32(&mut self) -> Option<f32> {
        Some(f32::from_le_bytes(self.take(4)?.try_into().ok()?))
    }
    fn f64(&mut self) -> Option<f64> {
        Some(f64::from_le_bytes(self.take(8)?.try_into().ok()?))
    }
}
