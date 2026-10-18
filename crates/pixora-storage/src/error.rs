use thiserror::Error;

#[derive(Debug, Error)]
pub enum StorageError {
    #[error("IO 错误：{0}")]
    Io(#[from] std::io::Error),

    #[error("对象不存在：{0}")]
    NotFound(String),

    #[error("驱动 {driver} 不支持该操作：{operation}")]
    Unsupported {
        driver: &'static str,
        operation: &'static str,
    },

    /// 远端后端的具体错误：HTTP 状态码 + 服务端返回的片段。
    /// 上层据 `retryable` 决定要不要回退本地。
    #[error("远端后端 {driver} 请求失败（HTTP {status}）：{message}")]
    Remote {
        driver: &'static str,
        status: u16,
        message: String,
        retryable: bool,
    },

    #[error("配置错误（{driver}）：{message}")]
    Config { driver: &'static str, message: String },

    #[error("响应格式无法解析（{driver}）：{message}")]
    Malformed { driver: &'static str, message: String },

    #[error("数据库错误：{0}")]
    Database(String),

    #[error("对象大小超出该后端上限：{size} > {limit}")]
    TooLarge { size: u64, limit: u64 },
}

impl StorageError {
    /// 是否值得重试或回退到本地。设计稿要求远端后端失败自动回退本地，
    /// 靠这个判断区分「临时故障」与「配置就不对」。
    pub fn is_retryable(&self) -> bool {
        match self {
            StorageError::Io(_) => true,
            StorageError::Remote { retryable, .. } => *retryable,
            StorageError::Database(_) => true,
            _ => false,
        }
    }

    pub fn remote(driver: &'static str, status: u16, message: impl Into<String>) -> Self {
        // 0 表示连接/超时这类没有拿到 HTTP 响应的网络错误：值得重试，也应当
        // 回退本地。5xx 与服务端限流同理；4xx 是请求本身有问题，重试不会变好。
        let retryable = status == 0 || status >= 500 || status == 429;
        Self::Remote {
            driver,
            status,
            message: message.into(),
            retryable,
        }
    }

    pub fn config(driver: &'static str, message: impl Into<String>) -> Self {
        Self::Config {
            driver,
            message: message.into(),
        }
    }

    pub fn malformed(driver: &'static str, message: impl Into<String>) -> Self {
        Self::Malformed {
            driver,
            message: message.into(),
        }
    }
}

pub type Result<T> = std::result::Result<T, StorageError>;
