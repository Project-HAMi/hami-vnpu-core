/// 全局共享内存路径的环境变量名
pub const ENV_GLOBAL_SHM_PATH: &str = "NPU_GLOBAL_SHM_PATH";
/// 本地共享内存路径的环境变量名
pub const ENV_LOCAL_SHM_PATH: &str = "NPU_LOCAL_SHM_PATH";
/// Pod 显存上限，单位沿用 MB（1024 * 1024 字节）；0 表示不限。
pub const ENV_MEMORY_LIMIT_MB: &str = "NPU_MEMORY_LIMIT_MB";
/// 算力配置百分比，整数 1..=100；具体执行方式由调度策略决定。
pub const ENV_COMPUTE_LIMIT: &str = "NPU_COMPUTE_LIMIT";
/// 调度策略：dynamic、fixed、overdraft 或 spatial。
pub const ENV_SCHED_POLICY: &str = "NPU_SCHED_POLICY";

pub const DEFAULT_LOCAL_SHMEM_PATH: &str = "/tmp/vnpu_local_shmem";
// 沿用旧配置的默认数值，避免仅改名就把每轮令牌数增加 100 倍。
pub const DEFAULT_COMPUTE_LIMIT: u32 = 1;
pub const DEFAULT_MEMORY_LIMIT_MB: u64 = 0;
pub const DEFAULT_SCHED_POLICY: SchedPolicy = SchedPolicy::Spatial;

#[derive(Debug, Clone, Copy)]
pub struct OverdraftConfig {
    pub limit: u32,
    pub timeout_us: u64,
    pub poll_interval_us: u64,
}

// Policy internals, not additional environment settings. Keep tuning in one place.
const OVERDRAFT_DEFAULTS: OverdraftConfig = OverdraftConfig {
    limit: 10,
    timeout_us: 5000,
    poll_interval_us: 1000,
};

const _: () = {
    assert!(
        OVERDRAFT_DEFAULTS.limit > 0 && OVERDRAFT_DEFAULTS.limit <= crate::overdraft::MAX_LIMIT
    );
    assert!(OVERDRAFT_DEFAULTS.timeout_us > 0 && OVERDRAFT_DEFAULTS.poll_interval_us > 0);
};

/// HBM虚拟开销预留大小（128MB）
pub const VIRTUAL_OVERHEAD_MB: usize = 128; // Reserve 128MB as HBM overhead

/// Named compute policies; every new policy must define its behavior explicitly.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SchedPolicy {
    /// 按配置数值分配相对份额，允许空闲资源借用，不提供绝对百分比上限。
    Dynamic,
    /// 按配置百分比计算运行/休息比例，沿用现有 fixed-share 算法。
    Fixed,
    /// Dynamic rotation plus a bounded allowance while this Pod is IDLE.
    Overdraft,
    /// Cooperative Cube/Vector core budgets; no time-sliced admission gate.
    Spatial,
}

impl SchedPolicy {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Dynamic => "dynamic",
            Self::Fixed => "fixed",
            Self::Overdraft => "overdraft",
            Self::Spatial => "spatial",
        }
    }

    pub fn is_fixed(self) -> bool {
        // Exhaustive matching forces new policies to choose their behavior.
        match self {
            Self::Dynamic | Self::Overdraft | Self::Spatial => false,
            Self::Fixed => true,
        }
    }

    pub fn is_time_sliced(self) -> bool {
        match self {
            Self::Dynamic | Self::Fixed | Self::Overdraft => true,
            Self::Spatial => false,
        }
    }

    /// Workers also need the policy before constructing timing resources.
    pub fn from_env() -> Result<Self, String> {
        match std::env::var(ENV_SCHED_POLICY) {
            Ok(value) => value.parse(),
            Err(std::env::VarError::NotPresent) => Ok(DEFAULT_SCHED_POLICY),
            Err(std::env::VarError::NotUnicode(_)) => {
                Err(format!("{ENV_SCHED_POLICY} must contain valid Unicode"))
            }
        }
    }
}

impl std::str::FromStr for SchedPolicy {
    type Err = String;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        match value.trim() {
            "dynamic" => Ok(Self::Dynamic),
            "fixed" => Ok(Self::Fixed),
            "overdraft" => Ok(Self::Overdraft),
            "spatial" => Ok(Self::Spatial),
            _ => Err(format!(
                "{ENV_SCHED_POLICY} must be dynamic, fixed, overdraft, or spatial"
            )),
        }
    }
}

impl std::fmt::Display for SchedPolicy {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(self.as_str())
    }
}

/// Pod 资源与调度配置，统一在此解析。
#[derive(Debug, Clone)]
pub struct ManagerConfig {
    pub global_shm_path: String,
    pub local_shm_path: String,
    /// Physical NPU assigned to this independent accounting/scheduling domain.
    pub physical_device_id: Option<u32>,
    pub compute_limit: u32,
    pub memory_limit_mb: u64,
    pub sched_policy: SchedPolicy,
    pub overdraft: OverdraftConfig,
}

impl ManagerConfig {
    /// Derive one domain per physical NPU, shared across logical-device remapping.
    pub fn for_device(&self, physical_device_id: u32) -> Self {
        let mut config = self.clone();
        let derive = |path: &str| {
            let base = self
                .physical_device_id
                .and_then(|previous| path.strip_suffix(&format!(".device-{previous}")))
                .unwrap_or(path);
            format!("{base}.device-{physical_device_id}")
        };
        config.global_shm_path = derive(&self.global_shm_path);
        config.local_shm_path = derive(&self.local_shm_path);
        config.physical_device_id = Some(physical_device_id);
        config
    }

    pub fn from_env() -> Result<Self, String> {
        Self::from_lookup(|name| match std::env::var(name) {
            Ok(value) => Ok(Some(value)),
            Err(std::env::VarError::NotPresent) => Ok(None),
            Err(std::env::VarError::NotUnicode(_)) => {
                Err(format!("{name} must contain valid Unicode"))
            }
        })
    }

    // Injectable lookup keeps configuration tests independent of process-global env.
    fn from_lookup(
        mut get: impl FnMut(&str) -> Result<Option<String>, String>,
    ) -> Result<Self, String> {
        for (old, new) in [
            ("NPU_MEM_QUOTA", ENV_MEMORY_LIMIT_MB),
            ("NPU_PRIORITY", ENV_COMPUTE_LIMIT),
            ("NPU_FIXED_SHARE_RATIO", ENV_SCHED_POLICY),
            ("NPU_OVERDRAFT_ENABLED", ENV_SCHED_POLICY),
        ] {
            if get(old)?.is_some() {
                return Err(format!(
                    "{old} has been replaced by {new}; remove the old variable"
                ));
            }
        }
        let global_shm_path = get(ENV_GLOBAL_SHM_PATH)?
            .filter(|value| !value.trim().is_empty())
            .ok_or_else(|| format!("Missing environment variable: {ENV_GLOBAL_SHM_PATH}"))?;
        let local_shm_path =
            get(ENV_LOCAL_SHM_PATH)?.unwrap_or_else(|| DEFAULT_LOCAL_SHMEM_PATH.to_string());
        if local_shm_path.trim().is_empty() {
            return Err(format!("{ENV_LOCAL_SHM_PATH} must not be empty"));
        }
        let memory_limit_mb = match get(ENV_MEMORY_LIMIT_MB)? {
            Some(value) => value
                .trim()
                .parse::<u64>()
                .map_err(|_| format!("{ENV_MEMORY_LIMIT_MB} must be a non-negative integer"))?,
            None => DEFAULT_MEMORY_LIMIT_MB,
        };
        if memory_limit_mb > u64::MAX / (1024 * 1024) {
            return Err(format!(
                "{ENV_MEMORY_LIMIT_MB} is too large to convert to bytes"
            ));
        }
        let compute_limit = match get(ENV_COMPUTE_LIMIT)? {
            Some(value) => value
                .trim()
                .parse::<u32>()
                .ok()
                .filter(|value| (1..=100).contains(value))
                .ok_or_else(|| format!("{ENV_COMPUTE_LIMIT} must be an integer in [1,100]"))?,
            None => DEFAULT_COMPUTE_LIMIT,
        };
        let sched_policy = match get(ENV_SCHED_POLICY)? {
            None => DEFAULT_SCHED_POLICY,
            Some(value) => value.parse::<SchedPolicy>()?,
        };
        let overdraft = OVERDRAFT_DEFAULTS;
        Ok(Self {
            global_shm_path,
            local_shm_path,
            physical_device_id: None,
            compute_limit,
            memory_limit_mb,
            sched_policy,
            overdraft,
        })
    }
}

/// Validate explicit resource settings before a worker can enter the stub path.
pub fn validate_requested_limits() -> Result<(), String> {
    let requested = [
        ENV_MEMORY_LIMIT_MB,
        ENV_COMPUTE_LIMIT,
        ENV_SCHED_POLICY,
        "NPU_MEM_QUOTA",
        "NPU_PRIORITY",
        "NPU_FIXED_SHARE_RATIO",
        "NPU_OVERDRAFT_ENABLED",
    ]
    .iter()
    .any(|name| std::env::var_os(name).is_some());
    if requested {
        ManagerConfig::from_env().map(|_| ())
    } else {
        Ok(())
    }
}

pub fn local_shmem_path() -> String {
    std::env::var(ENV_LOCAL_SHM_PATH).unwrap_or_else(|_| DEFAULT_LOCAL_SHMEM_PATH.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn config(values: &[(&str, &str)]) -> Result<ManagerConfig, String> {
        ManagerConfig::from_lookup(|name| {
            Ok(values
                .iter()
                .find(|(key, _)| *key == name)
                .map(|(_, value)| value.to_string()))
        })
    }

    #[test]
    fn physical_devices_have_independent_stable_domain_paths() {
        let base = config(&[
            (ENV_GLOBAL_SHM_PATH, "/shared/npu"),
            (ENV_LOCAL_SHM_PATH, "/tmp/pod"),
            (ENV_MEMORY_LIMIT_MB, "20480"),
            (ENV_COMPUTE_LIMIT, "30"),
            (ENV_SCHED_POLICY, "overdraft"),
        ])
        .unwrap();
        let first = base.for_device(7);
        let second = base.for_device(3);
        assert_eq!(first.global_shm_path, "/shared/npu.device-7");
        assert_eq!(first.local_shm_path, "/tmp/pod.device-7");
        assert_eq!(second.global_shm_path, "/shared/npu.device-3");
        assert_eq!(second.local_shm_path, "/tmp/pod.device-3");
        assert_eq!(first.physical_device_id, Some(7));
        assert_eq!(second.physical_device_id, Some(3));
        assert_eq!(first.memory_limit_mb, 20480);
        assert_eq!(first.compute_limit, 30);
        assert_eq!(first.sched_policy, SchedPolicy::Overdraft);
        assert_eq!(base.physical_device_id, None);
        assert_eq!(base.local_shm_path, "/tmp/pod");
        assert_eq!(first.for_device(7).local_shm_path, first.local_shm_path);
        assert_eq!(first.for_device(3).global_shm_path, second.global_shm_path);
        assert_eq!(first.for_device(3).local_shm_path, second.local_shm_path);
    }

    #[test]
    fn policy_names_round_trip_through_the_enum() {
        for policy in [
            SchedPolicy::Dynamic,
            SchedPolicy::Fixed,
            SchedPolicy::Overdraft,
            SchedPolicy::Spatial,
        ] {
            assert_eq!(policy.as_str().parse::<SchedPolicy>().unwrap(), policy);
            assert_eq!(policy.to_string(), policy.as_str());
        }
        assert!(!SchedPolicy::Dynamic.is_fixed());
        assert!(!SchedPolicy::Overdraft.is_fixed());
        assert!(SchedPolicy::Fixed.is_fixed());
        assert!(!SchedPolicy::Spatial.is_fixed());
        assert!(!SchedPolicy::Spatial.is_time_sliced());
        for policy in [
            SchedPolicy::Dynamic,
            SchedPolicy::Fixed,
            SchedPolicy::Overdraft,
        ] {
            assert!(policy.is_time_sliced());
        }
    }

    #[test]
    fn defaults_select_spatial_and_preserve_resource_values() {
        let cfg = config(&[(ENV_GLOBAL_SHM_PATH, "/tmp/global")]).unwrap();
        assert_eq!(cfg.compute_limit, 1);
        assert_eq!(cfg.memory_limit_mb, 0);
        assert_eq!(cfg.sched_policy, SchedPolicy::Spatial);
        assert_eq!(cfg.local_shm_path, DEFAULT_LOCAL_SHMEM_PATH);
    }

    #[test]
    fn new_variables_select_fixed_percent_and_memory_limit() {
        let cfg = config(&[
            (ENV_GLOBAL_SHM_PATH, "/tmp/global"),
            (ENV_LOCAL_SHM_PATH, "/tmp/pod"),
            (ENV_MEMORY_LIMIT_MB, "10240"),
            (ENV_COMPUTE_LIMIT, "30"),
            (ENV_SCHED_POLICY, "fixed"),
        ])
        .unwrap();
        assert_eq!(cfg.memory_limit_mb, 10240);
        assert_eq!(cfg.compute_limit, 30);
        assert_eq!(cfg.local_shm_path, "/tmp/pod");
        assert!(cfg.sched_policy.is_fixed());
        assert_eq!(cfg.sched_policy.as_str(), "fixed");
    }

    #[test]
    fn percentage_bounds_are_inclusive() {
        for value in ["1", "100"] {
            assert!(
                config(&[
                    (ENV_GLOBAL_SHM_PATH, "/tmp/global"),
                    (ENV_COMPUTE_LIMIT, value)
                ])
                .is_ok()
            );
        }
    }

    #[test]
    fn invalid_percentages_do_not_silently_fall_back() {
        for value in [
            "", "0", "101", "-1", "0.5", "30.5", "NaN", "inf", "30%", "abc",
        ] {
            let error = config(&[
                (ENV_GLOBAL_SHM_PATH, "/tmp/global"),
                (ENV_COMPUTE_LIMIT, value),
            ])
            .unwrap_err();
            assert!(error.contains(ENV_COMPUTE_LIMIT), "{value}: {error}");
        }
    }

    #[test]
    fn invalid_memory_limits_do_not_disable_the_limit() {
        for value in ["", "-1", "1.5", "10240MB", "18446744073709551615"] {
            let error = config(&[
                (ENV_GLOBAL_SHM_PATH, "/tmp/global"),
                (ENV_MEMORY_LIMIT_MB, value),
            ])
            .unwrap_err();
            assert!(error.contains(ENV_MEMORY_LIMIT_MB), "{value}: {error}");
        }
    }

    #[test]
    fn zero_memory_means_unlimited() {
        assert_eq!(
            config(&[
                (ENV_GLOBAL_SHM_PATH, "/tmp/global"),
                (ENV_MEMORY_LIMIT_MB, "0")
            ])
            .unwrap()
            .memory_limit_mb,
            0
        );
    }

    #[test]
    fn policy_is_explicit_instead_of_boolean() {
        for value in ["", "true", "false", "0", "1", "unknown"] {
            let error = config(&[
                (ENV_GLOBAL_SHM_PATH, "/tmp/global"),
                (ENV_SCHED_POLICY, value),
            ])
            .unwrap_err();
            assert!(error.contains(ENV_SCHED_POLICY));
        }
        assert_eq!(
            config(&[
                (ENV_GLOBAL_SHM_PATH, "/tmp/global"),
                (ENV_SCHED_POLICY, "dynamic")
            ])
            .unwrap()
            .sched_policy,
            SchedPolicy::Dynamic
        );
        assert_eq!(
            config(&[
                (ENV_GLOBAL_SHM_PATH, "/tmp/global"),
                (ENV_SCHED_POLICY, "spatial"),
                (ENV_COMPUTE_LIMIT, "33"),
                (ENV_MEMORY_LIMIT_MB, "20480")
            ])
            .unwrap()
            .sched_policy,
            SchedPolicy::Spatial
        );
    }

    #[test]
    fn old_names_report_their_replacement() {
        for (old, new) in [
            ("NPU_MEM_QUOTA", ENV_MEMORY_LIMIT_MB),
            ("NPU_PRIORITY", ENV_COMPUTE_LIMIT),
            ("NPU_FIXED_SHARE_RATIO", ENV_SCHED_POLICY),
        ] {
            let error = config(&[(ENV_GLOBAL_SHM_PATH, "/tmp/global"), (old, "1")]).unwrap_err();
            assert!(error.contains(old) && error.contains(new));
        }
    }

    #[test]
    fn shared_memory_paths_cannot_be_empty() {
        assert!(config(&[]).is_err());
        assert!(config(&[(ENV_GLOBAL_SHM_PATH, " ")]).is_err());
        assert!(
            config(&[
                (ENV_GLOBAL_SHM_PATH, "/tmp/global"),
                (ENV_LOCAL_SHM_PATH, "")
            ])
            .is_err()
        );
    }

    #[test]
    fn non_unicode_configuration_is_an_error() {
        let error = ManagerConfig::from_lookup(|name| {
            if name == ENV_MEMORY_LIMIT_MB {
                Err("invalid Unicode".into())
            } else if name == ENV_GLOBAL_SHM_PATH {
                Ok(Some("/tmp/global".into()))
            } else {
                Ok(None)
            }
        })
        .unwrap_err();
        assert!(error.contains("invalid Unicode"));
    }
    #[test]
    fn overdraft_uses_internal_defaults_without_more_environment_settings() {
        let cfg = ManagerConfig::from_lookup(|name| {
            assert!(
                [
                    ENV_GLOBAL_SHM_PATH,
                    ENV_LOCAL_SHM_PATH,
                    ENV_MEMORY_LIMIT_MB,
                    ENV_COMPUTE_LIMIT,
                    ENV_SCHED_POLICY,
                    "NPU_MEM_QUOTA",
                    "NPU_PRIORITY",
                    "NPU_FIXED_SHARE_RATIO",
                    "NPU_OVERDRAFT_ENABLED"
                ]
                .contains(&name),
                "unexpected environment setting: {name}"
            );
            Ok(match name {
                ENV_GLOBAL_SHM_PATH => Some("/tmp/global".into()),
                ENV_SCHED_POLICY => Some("overdraft".into()),
                _ => None,
            })
        })
        .unwrap();
        assert_eq!(cfg.sched_policy, SchedPolicy::Overdraft);
        assert_eq!(cfg.overdraft.limit, 10);
        assert_eq!(cfg.overdraft.timeout_us, 5000);
        assert_eq!(cfg.overdraft.poll_interval_us, 1000);
        assert!(
            config(&[
                (ENV_GLOBAL_SHM_PATH, "/tmp/global"),
                ("NPU_OVERDRAFT_ENABLED", "true")
            ])
            .is_err()
        );
    }
}
