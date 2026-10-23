//! Bounded, versioned output from the exec-isolated DCMI helper.

pub const MAX_DEVICES: usize = 8;
pub const MAX_PROCESSES: usize = 64;
pub const MAX_OUTPUT: usize = 32 * 1024;
pub const HEADER: &str = "VNPU_DCMI_1";
pub const READY: &str = "VNPU_DCMI_READY\n";
pub const HELPER_ENV: &str = "NPU_DCMI_HELPER_PROCESS";

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Record {
    pub device: usize,
    pub pid: i32,
    pub bytes: u64,
}

pub fn decode(bytes: &[u8]) -> Result<Vec<Record>, &'static str> {
    if bytes.len() > MAX_OUTPUT {
        return Err("oversized DCMI output");
    }
    let text = std::str::from_utf8(bytes).map_err(|_| "invalid DCMI encoding")?;
    let mut lines = text.lines();
    if lines.next() != Some(HEADER) || !text.ends_with('\n') {
        return Err("invalid DCMI header");
    }
    let mut records = Vec::new();
    for line in lines {
        let fields: Vec<_> = line.split_whitespace().collect();
        if fields.len() != 3 {
            return Err("invalid DCMI record");
        }
        let device = fields[0].parse::<usize>().map_err(|_| "invalid device")?;
        let pid = fields[1].parse::<i32>().map_err(|_| "invalid pid")?;
        let bytes = fields[2].parse::<u64>().map_err(|_| "invalid size")?;
        if device >= MAX_DEVICES || pid <= 0 {
            return Err("DCMI record out of range");
        }
        if records
            .iter()
            .any(|r: &Record| r.device == device && r.pid == pid)
        {
            return Err("duplicate DCMI record");
        }
        records.push(Record { device, pid, bytes });
        if records.len() > MAX_DEVICES * MAX_PROCESSES {
            return Err("too many DCMI records");
        }
    }
    Ok(records)
}
