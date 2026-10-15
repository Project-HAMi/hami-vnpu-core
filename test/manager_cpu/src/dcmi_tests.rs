use crate::dcmi::Collector;
use crate::dcmi_protocol::{Record, decode};
use std::os::unix::fs::PermissionsExt;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::{Duration, Instant};

struct Helper(PathBuf);
impl Helper {
    fn new(script: &str) -> Self {
        static NEXT: AtomicU64 = AtomicU64::new(0);
        let path = std::env::temp_dir().join(format!(
            "vnpu-helper-test-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::write(&path, format!("#!/bin/sh\nset -eu\n{script}\n")).unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o700)).unwrap();
        Self(path)
    }
}
impl Drop for Helper {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.0);
    }
}

#[test]
fn snapshot_protocol_rejects_malformed_duplicate_and_out_of_range_records() {
    for invalid in [
        b"".as_slice(),
        b"VNPU_DCMI_1",
        b"VNPU_DCMI_1\n8 2 3\n",
        b"VNPU_DCMI_1\n0 -1 3\n",
        b"VNPU_DCMI_1\n0 2 -1\n",
        b"VNPU_DCMI_1\n0 2 3 extra\n",
        b"VNPU_DCMI_1\n0 2 3\n0 2 4\n",
    ] {
        assert!(decode(invalid).is_err(), "accepted {invalid:?}");
    }
    assert_eq!(decode(b"VNPU_DCMI_1\n").unwrap(), vec![]);
}

#[test]
fn fresh_helper_receives_bypass_mode_and_returns_device_process_snapshot() {
    let _fixtures = crate::SUBPROCESS_FIXTURES.lock().unwrap();
    let helper = Helper::new(
        "[ \"$NPU_DCMI_HELPER_PROCESS\" = 1 ]\n[ -z \"${LD_PRELOAD+x}\" ]\nprintf 'VNPU_DCMI_1\\n0 123 4096\\n1 456 8192\\n'",
    );
    let result = Collector::default()
        .snapshot(&helper.0, Duration::from_secs(2), &AtomicBool::new(false))
        .unwrap();
    assert_eq!(
        result,
        vec![
            Record {
                device: 0,
                pid: 123,
                bytes: 4096
            },
            Record {
                device: 1,
                pid: 456,
                bytes: 8192
            }
        ]
    );
}

#[test]
fn a_valid_payload_does_not_hide_helper_failure() {
    let _fixtures = crate::SUBPROCESS_FIXTURES.lock().unwrap();
    let helper = Helper::new("printf 'VNPU_DCMI_1\\n0 123 4096\\n'\nexit 17");
    assert!(
        Collector::default()
            .snapshot(&helper.0, Duration::from_secs(2), &AtomicBool::new(false))
            .is_err()
    );
}

#[test]
fn oversized_output_cannot_block_on_a_full_pipe_or_allocate_without_bound() {
    let _fixtures = crate::SUBPROCESS_FIXTURES.lock().unwrap();
    let helper = Helper::new("exec head -c 65536 /dev/zero");
    let start = Instant::now();
    assert!(
        Collector::default()
            .snapshot(&helper.0, Duration::from_secs(1), &AtomicBool::new(false))
            .is_err()
    );
    assert!(start.elapsed() < Duration::from_secs(3));
}

#[test]
fn stalled_helper_and_its_descendant_are_terminated_and_next_query_can_progress() {
    let _fixtures = crate::SUBPROCESS_FIXTURES.lock().unwrap();
    let helper = Helper::new("exit 1");
    let pid_file = helper.0.with_extension("pids");
    std::fs::write(
        &helper.0,
        format!(
            "#!/bin/sh\ntrap 'kill \"$child\"; wait \"$child\"; exit 1' TERM\nsleep 30 &\nchild=$!\nprintf '%s %s\\n' \"$$\" \"$!\" > '{}'\nwait\n",
            pid_file.display()
        ),
    )
    .unwrap();
    let good = Helper::new("printf 'VNPU_DCMI_1\\n'");
    let mut collector = Collector::default();
    let start = Instant::now();
    let error = collector
        .snapshot(
            &helper.0,
            Duration::from_millis(500),
            &AtomicBool::new(false),
        )
        .unwrap_err();
    assert_eq!(error.kind(), std::io::ErrorKind::TimedOut);
    assert!(start.elapsed() < Duration::from_secs(2));
    let pids: Vec<i32> = std::fs::read_to_string(&pid_file)
        .unwrap()
        .split_whitespace()
        .map(|p| p.parse().unwrap())
        .collect();
    let _ = std::fs::remove_file(pid_file);
    let deadline = Instant::now() + Duration::from_secs(2);
    loop {
        if let Ok(records) =
            collector.snapshot(&good.0, Duration::from_secs(1), &AtomicBool::new(false))
        {
            assert!(records.is_empty());
            break;
        }
        assert!(Instant::now() < deadline, "killed helper was not reaped");
        std::thread::sleep(Duration::from_millis(10));
    }
    assert!(
        !std::path::Path::new(&format!("/proc/{}", pids[0])).exists(),
        "direct helper was not reaped"
    );
    loop {
        if !std::path::Path::new(&format!("/proc/{}", pids[1])).exists() {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "helper descendant survived group termination"
        );
        std::thread::sleep(Duration::from_millis(10));
    }
}

#[test]
fn lease_cancellation_interrupts_a_hung_query() {
    let _fixtures = crate::SUBPROCESS_FIXTURES.lock().unwrap();
    let helper = Helper::new(
        "trap 'kill \"$child\"; wait \"$child\"; exit 1' TERM\nsleep 30 &\nchild=$!\nwait \"$child\"",
    );
    let stop = AtomicBool::new(false);
    let start = Instant::now();
    std::thread::scope(|scope| {
        let query = scope
            .spawn(|| Collector::default().snapshot(&helper.0, Duration::from_secs(30), &stop));
        std::thread::sleep(Duration::from_millis(50));
        stop.store(true, Ordering::Release);
        assert!(query.join().unwrap().is_err());
    });
    assert!(start.elapsed() < Duration::from_secs(2));
}

#[test]
fn externally_reaped_cancelled_helper_does_not_disable_future_snapshots() {
    let _fixtures = crate::SUBPROCESS_FIXTURES.lock().unwrap();
    let helper = Helper::new("exit 1");
    let pid_file = helper.0.with_extension("pid");
    std::fs::write(
        &helper.0,
        format!(
            "#!/bin/sh\nprintf '%s\\n' \"$$\" > '{}'\nexec sleep 30\n",
            pid_file.display()
        ),
    )
    .unwrap();
    let mut collector = Collector::default();
    assert!(
        collector
            .snapshot(
                &helper.0,
                Duration::from_millis(100),
                &AtomicBool::new(false)
            )
            .is_err()
    );
    let pid: i32 = std::fs::read_to_string(&pid_file)
        .unwrap()
        .trim()
        .parse()
        .unwrap();
    std::fs::remove_file(pid_file).unwrap();
    let mut status = 0;
    assert_eq!(unsafe { libc::waitpid(pid, &mut status, 0) }, pid);
    let good = Helper::new("printf 'VNPU_DCMI_1\n'");
    assert!(
        collector
            .snapshot(&good.0, Duration::from_secs(1), &AtomicBool::new(false))
            .unwrap()
            .is_empty()
    );
}
