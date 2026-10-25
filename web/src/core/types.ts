/** 后端返回结构的前端类型定义。字段与 `docs/api-contract.md` 一一对应。 */

/** 本站开了哪些登录方式（`/site` 下发，登录页照着画按钮）。 */
export interface LoginMethods {
  github: boolean;
  google: boolean;
  qq: boolean;
  wechat: boolean;
  phone: boolean;
}

/** 账号挂着的一个登录方式。 */
export interface IdentityView {
  provider: string;
  label: string;
  display: string | null;
  created_at: string;
}

export interface IdentitiesPayload {
  email: string;
  /** 有没有设过密码：第三方建号的账号一开始没有 */
  password_set: boolean;
  identities: IdentityView[];
  available: LoginMethods;
}

export interface SiteInfo {
  name: string;
  base_url: string;
  upload_prefix: string;
  registration_open: boolean;
  registration_closed_reason: string | null;
  /** 允许注册的邮箱域名白名单；空数组 = 不限域名 */
  register_email_domains?: string[];
  plaza_public: boolean;
  formats: string[];
  max_file_bytes: number;
  max_batch_size: number;
  max_pixels: number;
  /** 长宽上限（像素），0 = 不限 */
  max_width: number;
  max_height: number;
  /** 客户端队列的最大长度：一次选太多会被截到这么多 */
  queue_depth: number;
  thumb_client: boolean;
  thumb_server_fallback: boolean;
  appeal_email: string | null;
  background: string | null;
  /** 站点默认头像：账号自己没设头像时用它，没配过是 null */
  default_avatar?: string | null;
  /** 支持的登录方式：登录页、注册页与账号安全页都读它 */
  login_methods?: LoginMethods;
  installed: boolean;
  terms: { required: boolean; version: string | null; articles: number };
  /** 广场网格列数，管理员可配 */
  plaza_grid_columns: number;
  /** 广场自动漫游间隔（秒）。0 = 关闭 */
  plaza_auto_refresh_seconds: number;
  /** 首页公告：打开广场后自动弹出；version 由内容算出，用户确认后本地记住 */
  announcement?: {
    enabled: boolean;
    title: string;
    body: string;
    version: string;
    /** 服务端已读（仅登录用户） */
    read?: boolean;
    starts_at?: string | null;
    ends_at?: string | null;
  } | null;
  /** 背景音乐：只有管理员启用后前端才显示播放按钮 */
  music?: {
    enabled: boolean;
    source: "api" | "local";
    autoplay?: boolean;
    /** 新访客默认音量，0～1；用户拖过音量后以本地记忆为准 */
    default_volume?: number;
  } | null;
  /** 回收站保留天数：0 = 关闭回收站（删除即彻底删除） */
  recycle_retention_days: number;
  /** 会话有效期：闲置多少分钟掉线、最长多少天 */
  session?: { idle_minutes: number; absolute_days: number };
  /**
   * 未登录也能上传时下发访客账号那套额度；关着就是 null。
   *
   * 客户端队列照着它限流，否则本地会排一堆、服务端一件件拒。
   */
  guest_upload?: {
    enabled: boolean;
    max_file_bytes: number;
    max_batch_size: number;
    max_concurrent_uploads: number;
    queue_depth: number;
    formats: string[];
  } | null;
}

export interface UserLimits {
  api_per_minute: number;
  upload_per_minute: number;
  upload_per_day: number;
  upload_bytes_per_day: number;
  /** 生效的上传策略（可能被管理员按用户覆盖过，不等于 /site 里的全局值） */
  max_file_size: number;
  max_batch_size: number;
  max_pixels: number;
  max_width: number;
  max_height: number;
  formats: string[];
  max_concurrent_uploads: number;
  /** 客户端队列的最大长度（服务端也按它卡预检，管理员同样受约束） */
  queue_depth: number;
  unlimited: boolean;
}

export interface UserStats {
  album_count: number;
  image_count: number;
  public_count: number;
  tag_count: number;
}

/** 管理员免登录切换用户时，说明「从谁切过来的」。 */
export interface ImpersonatorInfo {
  id: number;
  uid: string;
  email: string;
}

export interface UserProfile {
  id: number;
  uid: string;
  email: string;
  role: "admin" | "user";
  status: "pending" | "active" | "suspended" | "banned";
  status_reason?: string | null;
  status_changed_at?: string | null;
  email_verified: boolean;
  created_at: string;
  last_login_at: string | null;
  last_login_ip: string | null;
  totp_enabled: boolean;
  /** 管理员直接设过密码：改掉之前只放行 /me 与改密码，前端把人钉在改密码页 */
  must_change_password?: boolean;
  /** 头像：站内相对路径或外链；空 = 没设，界面回落到站点默认或内置翼标 */
  avatar?: string | null;
  login_alert: boolean;
  new_device_guard: boolean;
  recovery_codes_remaining: number;
  quota_bytes: number;
  used_bytes: number;
  stats: UserStats;
  limits: UserLimits;
  /**
   * 安装时创建的那个管理员账号的 UID（普通用户拿到的是 null）。
   *
   * 它是这套部署的主钥匙：别的管理员不能改它、切进去用或删掉。界面据此
   * 关掉那些入口——服务端本来也会拒，这里只是不让人白点一下。
   */
  install_admin_uid?: string | null;
}

/** 所有图片列表共用的形状。 */
export interface ImageItem {
  id: number;
  title: string | null;
  original_name: string | null;
  sha256: string;
  short_hash: string;
  ext: string;
  mime: string;
  size: number;
  width: number | null;
  height: number | null;
  album_id: number | null;
  album_name: string | null;
  owner_uid: string;
  owner_id: number;
  visibility: "public" | "private";
  views: number;
  upload_time: string;
  tags: string[];
  has_thumb: boolean;
  url: string;
  thumb_url: string | null;
}

export interface ImageDetail extends ImageItem {
  storage_driver: string;
  created_at: string;
  original_filename_visible: boolean;
  album: { id: number; name: string } | null;
  siblings: { id: number; thumb_url: string | null }[];
}

export interface Album {
  id: number;
  name: string;
  slug: string;
  description: string | null;
  visibility: "public" | "private";
  image_count: number;
  bytes: number;
  cover_url: string | null;
  sort_order: number;
  updated_at: string;
}

export interface AlbumList {
  all: { id: null; name: string; image_count: number; bytes: number };
  albums: Album[];
  usage: { used_bytes: number; quota_bytes: number };
}

export interface UploadResult {
  id: number;
  job_id: number;
  url: string;
  thumb_url: string | null;
  sha256: string;
  size: number;
  dedup: boolean;
  storage_type: string;
  fallback_message: string | null;
  tags: string[];
}

export interface JobStatus {
  id: number;
  status: "queued" | "running" | "done" | "skipped" | "failed";
  dedup: boolean;
  image_id: number | null;
  error: string | null;
}

export interface HashCheck {
  exist: string[];
  missing: string[];
}

export interface SessionRow {
  id: number;
  device: string | null;
  /** 用户自己起的备注（「书房的笔记本」这类） */
  label?: string | null;
  ip: string | null;
  created_at: string;
  last_seen_at: string;
  expires_at?: string | null;
  revoked_at?: string | null;
  current: boolean;
  /** 是否是「保持登录」签发的长期 cookie */
  remember?: boolean;
  /** active / saved / expired / revoked */
  state?: "active" | "saved" | "expired" | "revoked";
}

export interface ApiTokenRow {
  id: number;
  name: string;
  display: string;
  scopes: string[];
  last_used_at: string | null;
  expires_at: string | null;
  revoked_at: string | null;
  created_at: string;
  token?: string;
}

export interface TagRow {
  name: string;
  usage: number;
}

export interface TermsVersion {
  id: number;
  version: string;
  title: string;
  body: string;
  summary: string | null;
  articles: number;
  effective_at: string | null;
  published_at: string | null;
  status?: "draft" | "active" | "archived";
  status_label?: string;
  consent_count?: number;
  user_total?: number;
  agreed?: boolean;
}

export interface TermsPayload {
  current: TermsVersion | null;
  consent: { agreed_at: string; ip: string | null; version: string } | null;
  required: boolean;
  history: Array<{
    id: number;
    version: string;
    effective_at: string | null;
    summary: string | null;
    agreed: boolean;
  }>;
}

export interface AdminUserRow {
  /** 邮箱是否已验证：后台详情页的徽章读它 */
  email_verified?: boolean;
  /** 访客账号：不接受登录，详情页据此灰掉改邮箱、改角色、改密码 */
  guest?: boolean;
  id: number;
  uid: string;
  email: string;
  role: "admin" | "user";
  status: string;
  status_label: string;
  quota_bytes: number;
  used_bytes: number;
  image_count: number;
  rate_custom: boolean;
  created_at: string;
  last_login_at: string | null;
}

export interface UserActionRow {
  id: number;
  action: string;
  action_label: string;
  reason: string | null;
  detail: Record<string, unknown>;
  mail_sent: boolean;
  created_at: string;
  actor_email?: string | null;
}

export interface PolicyFields {
  api_per_minute: number;
  api_per_hour: number;
  api_read_per_minute: number;
  api_burst: number;
  upload_per_minute: number;
  upload_per_hour: number;
  upload_per_day: number;
  upload_bytes_per_day: number;
  max_concurrent_uploads: number;
  /** 上传策略：0 / 空数组表示「跟随上一层」（全局默认，再往上才是系统设置） */
  max_batch_size: number;
  max_file_size: number;
  max_pixels: number;
  max_width: number;
  max_height: number;
  formats: string[];
  /** 下载限额（带 `?download=1` 的直链请求）。0 = 跟随上一层，最上层 0 = 不限 */
  download_per_minute: number;
  download_per_day: number;
  unlimited: boolean;
  note?: string | null;
}

/** 某个用户生效的上传策略（已解析过三层，拿到的就是实际数字）。 */
export interface EffectiveUploadLimits {
  max_file_size: number;
  max_batch_size: number;
  max_pixels: number;
  max_width: number;
  max_height: number;
  formats: string[];
  max_concurrent_uploads: number;
}

export interface BackendRow {
  id: number;
  driver: string;
  label: string;
  is_default: boolean;
  enabled: boolean;
  capacity_mode: "auto" | "manual" | "hidden";
  capacity_total: number | null;
  createdAt?: string;
  config: Record<string, unknown>;
}

export interface StorageAdmin {
  default_driver: string;
  backends: BackendRow[];
  capacity: {
    mode: "auto" | "manual" | "hidden";
    total: number | null;
    /** 本项目占用（库内文件汇总） */
    used: number;
    /** 宿主机整盘已用 / 可用；只有自动探测可信时才有 */
    host_used: number | null;
    host_available: number | null;
    source: string;
    manual_total?: number | null;
    note?: string;
    probe: {
      fs_kind: "local" | "network" | "unknown";
      fs_name: string;
      fs_type: string;
      trustworthy: boolean;
      note: string;
    };
  };
  paths: {
    data_dir: string;
    upload_prefix: string;
    layout: string;
    source: string;
  };
  /**
   * 各后端的「在用」内容。count 是图片张数（不是实体条数），
   * bytes 是仍被引用的实体字节（原图 + 缩略图，每个实体只算一次）。
   */
  distribution: { driver: string; label: string; count: number; bytes: number }[];
  /** 回收站里还没过保留期的那部分 */
  binned: { count: number; bytes: number };
  /** 已经没人引用、等后台回收的实体 */
  pending: { count: number; bytes: number };
  totals: { count: number; bytes: number; dedup_saved_bytes: number };
  mimes?: { mime: string; count: number; bytes: number }[];
  trend?: { bucket: string; count: number }[];
  hourly?: { bucket: string; count: number }[];
  recent?: ImageItem[];
  average_size?: number;
  image_total?: number;
  /** 缩略图重算的进度（管理员点过「重算缩略图」之后才有意义） */
  thumb_rebuild?: { running: boolean; done: number; total: number };
}

export interface QuotaAdmin {
  capacity: {
    mode: "auto" | "manual" | "hidden";
    total: number | null;
    /** 本项目占用（库内文件汇总） */
    used: number;
    /** 宿主机整盘已用 / 可用；只有自动探测可信时才有，占用率按它算 */
    host_used: number | null;
    host_available: number | null;
    source: string;
    note?: string;
    updated_at?: string | null;
  };
  /** 可分配上限 = 系统总额 − 系统占用；拿不到整盘数字时就是系统总额 */
  allocatable: number;
  allocated: number;
  allocated_percent: number;
  remaining: number;
  default_quota_bytes: number;
  per_user_max: number;
  auto_close: {
    enabled: boolean;
    on: number;
    off: number;
    register_open: boolean;
    closed_reason: string | null;
    /** 管理员手动开过注册：已超阈值但不自动关闭 */
    manual_override: boolean;
    /** 占用率（百分比），算不出时是 null */
    percent: number | null;
  };
  top_users: AdminUserRow[];
}

export interface RateLimitAdmin {
  global: PolicyFields;
  /** 游客的下载限额（游客没有账号，这两格是站点设置） */
  download_policy: {
    anon_per_minute: number;
    anon_per_day: number;
  };
  /** 站点级上传策略（系统设置里那份）：全局输入改的就是它 */
  upload_policy: {
    max_file_bytes: number;
    max_batch_size: number;
    max_pixels: number;
    max_width: number;
    max_height: number;
    queue_depth: number;
    formats: string[];
  };
  on_breach: {
    respond: boolean;
    audit: boolean;
    temp_ban: boolean;
    threshold: number;
    threshold_minutes: number;
    ban_minutes: number;
  };
  queue_depth: { threshold: number; current: number };
  users: Array<{
    uid: string;
    email: string;
    role: string;
    status: string;
    custom: boolean;
    unlimited: boolean;
    api_per_minute: number;
    upload_per_minute: number;
    upload_per_day: number;
    upload_bytes_per_day: number;
    /** 生效的下载限额（含全局默认与用户覆盖解析后的值） */
    download_per_minute: number;
    download_per_day: number;
    /** 该用户生效的上传策略（三层解析后的结果） */
    upload?: EffectiveUploadLimits | null;
    hits_24h: number;
    policy?: PolicyFields | null;
  }>;
  realtime: Array<{
    uid: string;
    email: string;
    status: string;
    api_minute: number;
    api_minute_limit: number;
    upload_minute: number;
    upload_minute_limit: number;
    upload_day: number;
    upload_day_limit: number;
    hits_24h: number;
  }>;
}

export interface IpAdmin {
  policy: {
    enabled: boolean;
    trust_proxy: boolean;
    proxy_hops: number;
    auto_ban: boolean;
    fail_threshold: number;
    fail_window_minutes: number;
    ban_minutes: number;
  };
  rules: Array<{
    id: number;
    cidr: string;
    kind: "deny" | "allow";
    scope: string;
    scope_label: string;
    note: string | null;
    hits: number;
    is_auto: boolean;
    expires_at: string | null;
    created_at: string;
  }>;
  count: number;
}

export interface AdminSettings {
  site: {
    name: string;
    base_url: string;
    plaza_public: boolean;
    require_login_to_upload: boolean;
    register_enabled: boolean;
    /** 注册是否被「容量触顶自动关闭」关着（开关是它替管理员翻的） */
    register_auto_closed?: boolean;
    /** 自动关闭的具体口径（占用率、阈值），只有管理端能看到 */
    register_closed_reason?: string | null;
    /** 管理员是否手动开过注册（容量恢复前不会再自动关） */
    register_auto_close_override?: boolean;
    /** 自动关闭的阈值与当前占用率：手开之后也要说清「已超阈值」 */
    register_auto_close?: {
      enabled: boolean;
      on: number;
      off: number;
      percent: number | null;
    };
    /** 允许注册的邮箱域名白名单，逗号分隔；空 = 不限域名 */
    email_domains: string;
    plaza_grid_columns: number;
    /** 广场自动漫游间隔（秒）。0 = 关闭 */
    plaza_auto_refresh_seconds: number;
    /** 首页公告 */
    announcement?: {
      enabled: boolean;
      title: string;
      body: string;
      starts_at: string | null;
      ends_at: string | null;
      active: boolean;
      reads: number;
      user_total: number;
    };
    /** 只在广场展示这些公开相册；空数组 = 全部公开图片 */
    plaza_album_ids?: number[];
    /** 管理员命名的相册集合，随机图 API 可用 collection=名字 取 */
    plaza_collections?: Record<string, number[]>;
    recycle_retention_days: number;
  };
  upload: {
    max_file_bytes: number;
    max_batch_size: number;
    max_pixels: number;
    max_width: number;
    max_height: number;
    queue_depth: number;
    formats: string[];
    /** 可勾选的格式清单（服务端给，界面不再自己写死一份） */
    format_options?: Array<{ ext: string; label: string }>;
  };
  session: {
    idle_minutes: number;
    absolute_days: number;
    remember_days: number;
    /** 已结束的会话记录保留天数；0 = 永久保留 */
    history_retention_days: number;
  };
  security: {
    hotlink_enabled: boolean;
    hotlink_allow_empty: boolean;
    hotlink_domains: string;
  };
  moderation: {
    enabled: boolean;
    provider: string;
    threshold: number;
    score_field: string;
    endpoint: string;
    region: string;
    app_id: string;
    access_key_set: boolean;
    secret_key_set: boolean;
    token_set: boolean;
    user_enabled: boolean;
    admin_enabled: boolean;
    ban_after: number;
    ban_minutes: number;
    notify_admin: boolean;
    fail_open: boolean;
  };
  thumb: { client: boolean; server_fallback: boolean };
  mail: {
    smtp_host: string;
    smtp_port: number;
    smtp_security: string;
    smtp_user: string;
    password_set: boolean;
    from_name: string;
    from_email: string;
    appeal_email: string;
    disposable_check: boolean;
    mx_check: boolean;
    third_party_check: boolean;
    third_party_set: boolean;
  };
  appearance: { background: string | null; default_avatar: string | null };
  /** 管理后台挑「首页展示相册」用的公开相册清单 */
  plaza_albums?: Array<{
    id: number;
    name: string;
    slug: string;
    owner_uid: string;
    owner_email: string;
    image_count: number;
  }>;
  music?: {
    enabled: boolean;
    source: string;
    /** 旧字段；新界面用 api_urls */
    api_url: string;
    /** 有序列表：第一个是主 API */
    api_urls: string[];
    platform: string;
    playlist_id: string;
    local_dir: string;
    /** 环境变量 PIXORA_MUSIC_DIR 指定的容器内路径；有值时忽略 local_dir */
    local_dir_env: string | null;
    autoplay: boolean;
    /** 新访客默认音量，0～1 */
    default_volume: number;
  };
}

/** 背景音乐的一条曲目。 */
export interface MusicTrack {
  id: string;
  title: string;
  artist: string;
  album: string;
  duration_seconds: number;
  cover_url: string | null;
  audio_url: string;
  lyric_url: string | null;
  lyrics: string | null;
}

export interface MusicPlaylist {
  enabled: boolean;
  source: string;
  tracks: MusicTrack[];
  message: string | null;
}

export interface AdminStats {
  user_total: number;
  user_new_this_month: number;
  allocated_quota: number;
  used_bytes: number;
  image_total: number;
  storage_bytes: number;
  register_open: boolean;
}

export interface InstallStatus {
  installed: boolean;
  step: number;
  env: {
    data_dir: string;
    env_source: string;
    db_driver: string;
    db_dsn_set: boolean;
    master_key_set: boolean;
  };
  tables: number;
}

export interface FsTree {
  uid: string;
  path: string;
  files: number;
  size: number;
  nodes: Array<{
    type: "album" | "root";
    id: number | null;
    name: string;
    slug: string;
    files: number;
    size: number;
    images: Array<{ id: number; name: string; size: number; upload_time: string }>;
  }>;
  next_cursor: string | null;
}
