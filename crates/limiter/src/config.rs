/// 全局共享内存路径的环境变量名
pub const ENV_GLOBAL_SHM_PATH: &str = "NPU_GLOBAL_SHM_PATH";
/// 本地共享内存路径的环境变量名
pub const ENV_LOCAL_SHM_PATH: &str = "NPU_LOCAL_SHM_PATH";
/// Pod 显存上限，单位沿用 MB（1024 * 1024 字节）；0 表示不限。
pub const ENV_MEMORY_LIMIT_MB: &str = "NPU_MEMORY_LIMIT_MB";
/// 算力配置百分比，整数 1..=100；具体执行方式由调度策略决定。
pub const ENV_COMPUTE_LIMIT: &str = "NPU_COMPUTE_LIMIT";
/// 调度策略：dynamic（动态份额）或 fixed（固定比例）。
pub const ENV_SCHED_POLICY: &str = "NPU_SCHED_POLICY";

pub const DEFAULT_LOCAL_SHMEM_PATH: &str = "/tmp/vnpu_local_shmem";
// 沿用旧配置的默认数值，避免仅改名就把每轮令牌数增加 100 倍。
pub const DEFAULT_COMPUTE_LIMIT: u32 = 1;
pub const DEFAULT_MEMORY_LIMIT_MB: u64 = 0;
pub const DEFAULT_SCHED_POLICY: SchedPolicy = SchedPolicy::Dynamic;

/// HBM虚拟开销预留大小（128MB）
pub const VIRTUAL_OVERHEAD_MB: usize = 128; // Reserve 128MB as HBM overhead

/// 两种现有时分调度策略。空间切分后端不由这个枚举隐式接入。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SchedPolicy {
    /// 按配置数值分配相对份额，允许空闲资源借用，不提供绝对百分比上限。
    Dynamic,
    /// 按配置百分比计算运行/休息比例，沿用现有 fixed-share 算法。
    Fixed,
}

impl SchedPolicy {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Dynamic => "dynamic",
            Self::Fixed => "fixed",
        }
    }

    pub fn is_fixed(self) -> bool {
        // Exhaustive matching forces new policies to choose their behavior.
        match self {
            Self::Dynamic => false,
            Self::Fixed => true,
        }
    }
}

impl std::str::FromStr for SchedPolicy {
    type Err = String;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        match value.trim() {
            "dynamic" => Ok(Self::Dynamic),
            "fixed" => Ok(Self::Fixed),
            _ => Err(format!("{ENV_SCHED_POLICY} must be dynamic or fixed")),
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
    pub compute_limit: u32,
    pub memory_limit_mb: u64,
    pub sched_policy: SchedPolicy,
}

impl ManagerConfig {
    pub fn from_env() -> Result<Self, String> {
        Self::from_lookup(|name| match std::env::var(name) {
            Ok(value) => Ok(Some(value)),
            Err(std::env::VarError::NotPresent) => Ok(None),
            Err(std::env::VarError::NotUnicode(_)) => Err(format!("{name} must contain valid Unicode")),
        })
    }

    // Injectable lookup keeps configuration tests independent of process-global env.
    fn from_lookup(mut get: impl FnMut(&str) -> Result<Option<String>, String>) -> Result<Self, String> {
        for (old, new) in [
            ("NPU_MEM_QUOTA", ENV_MEMORY_LIMIT_MB),
            ("NPU_PRIORITY", ENV_COMPUTE_LIMIT),
            ("NPU_FIXED_SHARE_RATIO", ENV_SCHED_POLICY),
        ] {
            if get(old)?.is_some() {
                return Err(format!("{old} has been replaced by {new}; remove the old variable"));
            }
        }
        let global_shm_path = get(ENV_GLOBAL_SHM_PATH)?
            .filter(|value| !value.trim().is_empty())
            .ok_or_else(|| format!("Missing environment variable: {ENV_GLOBAL_SHM_PATH}"))?;
        let local_shm_path = get(ENV_LOCAL_SHM_PATH)?
            .unwrap_or_else(|| DEFAULT_LOCAL_SHMEM_PATH.to_string());
        if local_shm_path.trim().is_empty() {
            return Err(format!("{ENV_LOCAL_SHM_PATH} must not be empty"));
        }
        let memory_limit_mb = match get(ENV_MEMORY_LIMIT_MB)? {
            Some(value) => value.trim().parse::<u64>()
                .map_err(|_| format!("{ENV_MEMORY_LIMIT_MB} must be a non-negative integer"))?,
            None => DEFAULT_MEMORY_LIMIT_MB,
        };
        if memory_limit_mb > u64::MAX / (1024 * 1024) {
            return Err(format!("{ENV_MEMORY_LIMIT_MB} is too large to convert to bytes"));
        }
        let compute_limit = match get(ENV_COMPUTE_LIMIT)? {
            Some(value) => value.trim().parse::<u32>()
                .ok().filter(|value| (1..=100).contains(value))
                .ok_or_else(|| format!("{ENV_COMPUTE_LIMIT} must be an integer in [1,100]"))?,
            None => DEFAULT_COMPUTE_LIMIT,
        };
        let sched_policy = match get(ENV_SCHED_POLICY)? {
            None => DEFAULT_SCHED_POLICY,
            Some(value) => value.parse::<SchedPolicy>()?,
        };
        Ok(Self { global_shm_path, local_shm_path, compute_limit, memory_limit_mb, sched_policy })
    }
}

/// Validate explicit resource settings before a worker can enter the stub path.
pub fn validate_requested_limits() -> Result<(), String> {
    let requested = [ENV_MEMORY_LIMIT_MB, ENV_COMPUTE_LIMIT, ENV_SCHED_POLICY,
        "NPU_MEM_QUOTA", "NPU_PRIORITY", "NPU_FIXED_SHARE_RATIO"]
        .iter().any(|name| std::env::var_os(name).is_some());
    if requested {
        ManagerConfig::from_env().map(|_| ())
    } else {
        Ok(())
    }
}

pub fn local_shmem_path() -> String {
    std::env::var(ENV_LOCAL_SHM_PATH)
        .unwrap_or_else(|_| DEFAULT_LOCAL_SHMEM_PATH.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn config(values: &[(&str, &str)]) -> Result<ManagerConfig, String> {
        ManagerConfig::from_lookup(|name| Ok(values.iter().find(|(key, _)| *key == name)
            .map(|(_, value)| value.to_string())))
    }

    #[test]
    fn policy_names_round_trip_through_the_enum() {
        for policy in [SchedPolicy::Dynamic, SchedPolicy::Fixed] {
            assert_eq!(policy.as_str().parse::<SchedPolicy>().unwrap(), policy);
            assert_eq!(policy.to_string(), policy.as_str());
        }
        assert!(!SchedPolicy::Dynamic.is_fixed());
        assert!(SchedPolicy::Fixed.is_fixed());
    }

    #[test]
    fn defaults_preserve_existing_scheduling_granularity() {
        let cfg = config(&[(ENV_GLOBAL_SHM_PATH, "/tmp/global")]).unwrap();
        assert_eq!(cfg.compute_limit, 1);
        assert_eq!(cfg.memory_limit_mb, 0);
        assert_eq!(cfg.sched_policy, SchedPolicy::Dynamic);
        assert_eq!(cfg.local_shm_path, DEFAULT_LOCAL_SHMEM_PATH);
    }

    #[test]
    fn new_variables_select_fixed_percent_and_memory_limit() {
        let cfg = config(&[(ENV_GLOBAL_SHM_PATH, "/tmp/global"),
            (ENV_LOCAL_SHM_PATH, "/tmp/pod"), (ENV_MEMORY_LIMIT_MB, "10240"),
            (ENV_COMPUTE_LIMIT, "30"), (ENV_SCHED_POLICY, "fixed")]).unwrap();
        assert_eq!(cfg.memory_limit_mb, 10240);
        assert_eq!(cfg.compute_limit, 30);
        assert_eq!(cfg.local_shm_path, "/tmp/pod");
        assert!(cfg.sched_policy.is_fixed());
        assert_eq!(cfg.sched_policy.as_str(), "fixed");
    }

    #[test]
    fn percentage_bounds_are_inclusive() {
        for value in ["1", "100"] {
            assert!(config(&[(ENV_GLOBAL_SHM_PATH, "/tmp/global"), (ENV_COMPUTE_LIMIT, value)]).is_ok());
        }
    }

    #[test]
    fn invalid_percentages_do_not_silently_fall_back() {
        for value in ["", "0", "101", "-1", "0.5", "30.5", "NaN", "inf", "30%", "abc"] {
            let error = config(&[(ENV_GLOBAL_SHM_PATH, "/tmp/global"), (ENV_COMPUTE_LIMIT, value)]).unwrap_err();
            assert!(error.contains(ENV_COMPUTE_LIMIT), "{value}: {error}");
        }
    }

    #[test]
    fn invalid_memory_limits_do_not_disable_the_limit() {
        for value in ["", "-1", "1.5", "10240MB", "18446744073709551615"] {
            let error = config(&[(ENV_GLOBAL_SHM_PATH, "/tmp/global"), (ENV_MEMORY_LIMIT_MB, value)]).unwrap_err();
            assert!(error.contains(ENV_MEMORY_LIMIT_MB), "{value}: {error}");
        }
    }

    #[test]
    fn zero_memory_means_unlimited() {
        assert_eq!(config(&[(ENV_GLOBAL_SHM_PATH, "/tmp/global"), (ENV_MEMORY_LIMIT_MB, "0")]).unwrap().memory_limit_mb, 0);
    }

    #[test]
    fn policy_is_explicit_instead_of_boolean() {
        for value in ["", "true", "false", "0", "1", "spatial", "unknown"] {
            let error = config(&[(ENV_GLOBAL_SHM_PATH, "/tmp/global"), (ENV_SCHED_POLICY, value)]).unwrap_err();
            assert!(error.contains(ENV_SCHED_POLICY));
        }
        assert_eq!(config(&[(ENV_GLOBAL_SHM_PATH, "/tmp/global"), (ENV_SCHED_POLICY, "dynamic")]).unwrap().sched_policy, SchedPolicy::Dynamic);
    }

    #[test]
    fn old_names_report_their_replacement() {
        for (old, new) in [("NPU_MEM_QUOTA", ENV_MEMORY_LIMIT_MB),
            ("NPU_PRIORITY", ENV_COMPUTE_LIMIT), ("NPU_FIXED_SHARE_RATIO", ENV_SCHED_POLICY)] {
            let error = config(&[(ENV_GLOBAL_SHM_PATH, "/tmp/global"), (old, "1")]).unwrap_err();
            assert!(error.contains(old) && error.contains(new));
        }
    }

    #[test]
    fn shared_memory_paths_cannot_be_empty() {
        assert!(config(&[]).is_err());
        assert!(config(&[(ENV_GLOBAL_SHM_PATH, " ")]).is_err());
        assert!(config(&[(ENV_GLOBAL_SHM_PATH, "/tmp/global"), (ENV_LOCAL_SHM_PATH, "")]).is_err());
    }

    #[test]
    fn non_unicode_configuration_is_an_error() {
        let error = ManagerConfig::from_lookup(|name| {
            if name == ENV_MEMORY_LIMIT_MB { Err("invalid Unicode".into()) }
            else if name == ENV_GLOBAL_SHM_PATH { Ok(Some("/tmp/global".into())) }
            else { Ok(None) }
        }).unwrap_err();
        assert!(error.contains("invalid Unicode"));
    }
}
