/** 后端返回结构的前端类型定义。字段与 `docs/api-contract.md` 一一对应。 */

export interface SiteInfo {
  name: string;
  base_url: string;
  upload_prefix: string;
  registration_open: boolean;
  registration_closed_reason: string | null;
  plaza_public: boolean;
  formats: string[];
  max_file_bytes: number;
  max_batch_size: number;
  max_pixels: number;
  thumb_client: boolean;
  thumb_server_fallback: boolean;
  appeal_email: string | null;
  background: string | null;
  installed: boolean;
  terms: { required: boolean; version: string | null; articles: number };
  /** 广场网格列数，管理员可配 */
  plaza_grid_columns: number;
  /** 回收站保留天数：0 = 关闭回收站（删除即彻底删除） */
  recycle_retention_days: number;
}

export interface UserLimits {
  api_per_minute: number;
  upload_per_minute: number;
  upload_per_day: number;
  upload_bytes_per_day: number;
  max_file_size: number;
  max_batch_size: number;
  unlimited: boolean;
}

export interface UserStats {
  album_count: number;
  image_count: number;
  public_count: number;
  tag_count: number;
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
  login_alert: boolean;
  new_device_guard: boolean;
  recovery_codes_remaining: number;
  quota_bytes: number;
  used_bytes: number;
  stats: UserStats;
  limits: UserLimits;
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
  ip: string | null;
  created_at: string;
  last_seen_at: string;
  current: boolean;
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
  max_batch_size: number;
  max_file_size: number;
  unlimited: boolean;
  note?: string | null;
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
  distribution: { driver: string; label: string; count: number; bytes: number }[];
  totals: { count: number; bytes: number; dedup_saved_bytes: number };
  mimes?: { mime: string; count: number; bytes: number }[];
  trend?: { bucket: string; count: number }[];
  hourly?: { bucket: string; count: number }[];
  recent?: ImageItem[];
  average_size?: number;
  image_total?: number;
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
  };
  top_users: AdminUserRow[];
}

export interface RateLimitAdmin {
  global: PolicyFields;
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
    plaza_grid_columns: number;
    recycle_retention_days: number;
  };
  upload: {
    max_file_bytes: number;
    max_batch_size: number;
    max_pixels: number;
    queue_depth: number;
    formats: string[];
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
  appearance: { background: string | null };
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
