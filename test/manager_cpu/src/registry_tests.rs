use crate::registry_lock::RegistryLock;
use std::path::{Path, PathBuf};
use std::process::{Child, Command};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

struct Fixture(PathBuf);
impl Fixture {
    fn new() -> Self {
        static NEXT: AtomicU64 = AtomicU64::new(0);
        let path = std::env::temp_dir().join(format!(
            "vnpu-registry-test-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir(&path).unwrap();
        std::fs::File::create(path.join("registry")).unwrap();
        Self(path)
    }
    fn child(&self, mode: &str) -> Child {
        Command::new(std::env::current_exe().unwrap())
            .args([
                "--ignored",
                "--exact",
                "registry_tests::registry_child",
                "--test-threads=1",
            ])
            .env("VNPU_TEST_REGISTRY_DIR", &self.0)
            .env("VNPU_TEST_REGISTRY_MODE", mode)
            .stdout(std::process::Stdio::null())
            .spawn()
            .unwrap()
    }
}
impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn wait_file(path: &Path) {
    let deadline = Instant::now() + Duration::from_secs(3);
    while !path.exists() {
        assert!(
            Instant::now() < deadline,
            "child did not create {}",
            path.display()
        );
        std::thread::sleep(Duration::from_millis(5));
    }
}

fn wait_child(child: &mut Child) {
    let deadline = Instant::now() + Duration::from_secs(3);
    loop {
        if let Some(status) = child.try_wait().unwrap() {
            assert!(status.success());
            return;
        }
        if Instant::now() >= deadline {
            let _ = child.kill();
            let _ = child.wait();
            panic!("registry child did not progress");
        }
        std::thread::sleep(Duration::from_millis(5));
    }
}

#[test]
#[ignore = "launched in an isolated subprocess by the registry tests"]
fn registry_child() {
    let path = PathBuf::from(std::env::var_os("VNPU_TEST_REGISTRY_DIR").unwrap());
    let mode = std::env::var("VNPU_TEST_REGISTRY_MODE").unwrap();
    if mode == "invalid-paths" {
        unsafe {
            std::env::set_var("NPU_GLOBAL_SHM_PATH", path.join("registry"));
            std::env::set_var("NPU_LOCAL_SHM_PATH", path.join("registry"));
        }
        crate::supervisor::run();
        panic!("supervisor accepted overlapping mappings");
    }
    let registry_path = path.join("registry");
    let lock = RegistryLock::open(registry_path.to_str().unwrap()).unwrap();
    std::fs::write(path.join(format!("{mode}-started")), b"started").unwrap();
    let _guard = lock.lock().unwrap();
    if mode == "hold" {
        // Simulate an unrelated application thread forking while a transaction
        // owns its fd. The descendant intentionally retains that open fd.
        for fd in std::fs::read_dir("/proc/self/fd").unwrap().flatten() {
            if std::fs::read_link(fd.path()).ok().as_ref() == Some(&registry_path) {
                let fd: i32 = fd.file_name().to_string_lossy().parse().unwrap();
                assert_eq!(unsafe { libc::fcntl(fd, libc::F_SETFD, 0) }, 0);
            }
        }
        let child = Command::new("sleep").arg("30").spawn().unwrap();
        std::fs::write(path.join("held"), child.id().to_string()).unwrap();
        loop {
            std::thread::sleep(Duration::from_secs(1));
        }
    }
    std::fs::write(path.join("acquired"), b"acquired").unwrap();
}

#[test]
fn shared_memory_paths_reject_aliases_before_creation() {
    use crate::config::validate_shmem_paths;
    let fixture = Fixture::new();
    let root = &fixture.0;
    std::os::unix::fs::symlink(root, root.join("alias")).unwrap();
    std::os::unix::fs::symlink("new/registry", root.join("dangling")).unwrap();
    std::fs::hard_link(root.join("registry"), root.join("hardlink")).unwrap();
    std::fs::create_dir(root.join("sub")).unwrap();
    for (global, local) in [
        ("new/registry", "new/registry"),
        ("new/registry", "new/./registry"),
        ("new/registry", "alias/new/registry"),
        ("new/registry", "dangling"),
        ("registry", "hardlink"),
        ("registry", "alias/registry"),
        ("registry", "sub/../registry"),
    ] {
        let error = validate_shmem_paths(
            root.join(global).to_str().unwrap(),
            root.join(local).to_str().unwrap(),
        )
        .unwrap_err();
        assert_eq!(error.kind(), std::io::ErrorKind::InvalidInput);
    }
    validate_shmem_paths(
        root.join("new/global").to_str().unwrap(),
        root.join("new/local").to_str().unwrap(),
    )
    .unwrap();
    assert!(!root.join("new").exists(), "validation created files");
}

#[test]
fn supervisor_rejects_identical_paths_without_truncating_the_registry() {
    let _fixtures = crate::SUBPROCESS_FIXTURES.lock().unwrap();
    let fixture = Fixture::new();
    std::fs::write(fixture.0.join("registry"), b"unchanged registry").unwrap();
    let mut child = fixture.child("invalid-paths");
    let deadline = Instant::now() + Duration::from_secs(6);
    let status = loop {
        if let Some(status) = child.try_wait().unwrap() {
            break status;
        }
        if Instant::now() >= deadline {
            let _ = child.kill();
            let _ = child.wait();
            panic!("supervisor did not reject invalid paths");
        }
        std::thread::sleep(Duration::from_millis(10));
    };
    assert_eq!(status.code(), Some(78));
    assert_eq!(
        std::fs::read(fixture.0.join("registry")).unwrap(),
        b"unchanged registry"
    );
}

#[test]
fn registry_transaction_serializes_different_processes() {
    let _fixtures = crate::SUBPROCESS_FIXTURES.lock().unwrap();
    let fixture = Fixture::new();
    let lock = RegistryLock::open(fixture.0.join("registry").to_str().unwrap()).unwrap();
    let guard = lock.lock().unwrap();
    let mut child = fixture.child("acquire");
    wait_file(&fixture.0.join("acquire-started"));
    std::thread::sleep(Duration::from_millis(50));
    assert!(!fixture.0.join("acquired").exists());
    assert!(child.try_wait().unwrap().is_none());
    drop(guard);
    wait_child(&mut child);
    assert!(fixture.0.join("acquired").exists());
}

#[test]
fn owner_death_releases_transaction_even_if_a_descendant_retains_its_fd() {
    let _fixtures = crate::SUBPROCESS_FIXTURES.lock().unwrap();
    let fixture = Fixture::new();
    let mut owner = fixture.child("hold");
    wait_file(&fixture.0.join("held"));
    let descendant: i32 = std::fs::read_to_string(fixture.0.join("held"))
        .unwrap()
        .parse()
        .unwrap();
    assert_eq!(unsafe { libc::kill(descendant, 0) }, 0);
    owner.kill().unwrap();
    owner.wait().unwrap();
    let mut successor = fixture.child("acquire");
    wait_child(&mut successor);
    assert!(fixture.0.join("acquired").exists());
    // The descendant was alive during acquisition; close its test-only lifetime.
    assert_eq!(unsafe { libc::kill(descendant, libc::SIGKILL) }, 0);
}
