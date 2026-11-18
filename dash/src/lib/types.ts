// Shapes of the API answers the dashboard reads. They follow Cloudflare's API,
// plus Selvedge's /selvedge/ extensions.

export type Account = { id: string; name: string };

export type User = {
  id: string;
  email: string | null;
  name: string | null;
  admin: boolean;
  created_on: string;
  last_login_on: string;
};

export type Me = {
  user: User | null;
  token: { name: string } | null;
  admin: boolean;
  accounts: Account[];
};

export type AuthConfig = { oidc: { name: string } | null; token_login: boolean; workers_dev_zone: string | null };

export type Script = {
  id: string;
  created_on: string;
  modified_on: string;
  has_assets: boolean;
  handlers: string[];
};

export type Binding = { name: string; type: string } & Record<string, unknown>;

export type ScriptSettings = { bindings: Binding[] };

export type Version = {
  id: string;
  number: number;
  metadata: { created_on: string; source: string; author_email: string };
  annotations?: Record<string, string>;
  resources: {
    bindings: Binding[];
    script_runtime: { compatibility_date: string; compatibility_flags: string[] };
  };
};

export type Deployment = {
  id: string;
  created_on: string;
  source: string;
  strategy: string;
  annotations?: Record<string, string>;
  versions: { version_id: string; percentage: number }[];
};

export type Schedule = { cron: string; created_on: string; modified_on: string };

export type Route = { id: string; pattern: string; script?: string | null; zone_id?: string; zone_name?: string };

export type Domain = { id: string; hostname: string; service: string; zone_id: string; zone_name: string; environment?: string };

export type Zone = { id: string; name: string; account: { id: string; name?: string }; status?: string };

export type KvNamespace = { id: string; title: string };
export type KvKey = { name: string; expiration?: number; metadata?: unknown };

export type D1Database = { uuid: string; name: string; created_at: string; file_size: number; num_tables: number };
export type D1Result = {
  results?: Record<string, unknown>[];
  success: boolean;
  meta?: { duration?: number; rows_read?: number; rows_written?: number; changes?: number; last_row_id?: number };
};
export type D1Raw = { results: { columns: string[]; rows: unknown[][] }; success: boolean; meta?: D1Result["meta"] };

export type Bucket = { name: string; creation_date: string; location?: string; storage_class?: string };
export type R2Object = {
  key: string;
  size: number;
  etag: string;
  last_modified: string;
  storage_class?: string;
  http_metadata?: Record<string, string>;
  custom_metadata?: Record<string, string>;
};
export type R2ListInfo = { cursor: string | null; is_truncated: boolean; delimited?: string[] };
export type R2DevUrl = { domain: string; enabled: boolean };
export type R2CustomDomain = { domain: string; enabled: boolean; zoneId: string; zoneName: string };
export type R2NotificationQueue = {
  queueId: string;
  queueName: string;
  rules: { ruleId: string; prefix?: string; suffix?: string; actions: string[]; description?: string; createdAt?: string }[];
};

export type Consumer = {
  consumer_id: string;
  script_name?: string;
  type: string;
  settings: { batch_size?: number; max_retries?: number; max_wait_time_ms?: number; retry_delay?: number };
  dead_letter_queue?: string | null;
};
export type Queue = {
  queue_id: string;
  queue_name: string;
  created_on: string;
  modified_on: string;
  consumers: Consumer[];
  settings: { delivery_delay?: number; delivery_paused?: boolean; message_retention_period?: number };
  message_backlog_count?: number;
};

export type Workflow = {
  id: string;
  name: string;
  script_name: string;
  class_name: string;
  created_on: string;
  modified_on: string;
};
export type WorkflowInstance = { id: string; status: string; created_on: string; modified_on?: string };

export type ContainerApp = {
  id: string;
  name: string;
  instances: number;
  max_instances?: number;
  created_at: string;
  configuration?: { image?: string; instance_type?: string };
  durable_objects?: { namespace_id: string };
};

export type DoNamespace = { id: string; name: string; script: string; class: string; use_sqlite: boolean };

export type Member = { id: string; user: User | null; email: string | null; created_on: string };
export type Token = {
  id: string;
  name: string;
  created_on: string;
  created_by: User | null;
  creator_is_member: boolean | null;
};
export type NewToken = {
  id: string;
  name: string;
  value: string;
  s3: { access_key_id: string; secret_access_key: string };
};

export type TailSession = { id: string; url: string; expires_at: string };
