export type ChannelStatus = "ok" | "warning" | "critical";
export type AlertSeverity = "info" | "warning" | "critical";

export interface ChannelMetrics {
  requests: number;
  okRequests: number;
  errRequests: number;
  successRate: number | null;
  anomalyRate: number | null;
  avgLatencyMs: number | null;
  tokens: number;
  promptTokens: number;
  completionTokens: number;
  cacheHitRate: number | null;
  totalAccounts: number;
  healthyAccounts: number;
  coolingAccounts: number;
  errorAccounts: number;
  todayCheckedIn: number;
  checkinAccounts: number;
  modelCount: number;
}

export interface ChannelAlert {
  channelId: string;
  channelName: string;
  severity: AlertSeverity;
  metric: string;
  value: number | null;
  threshold: number | null;
  message: string;
}

export interface ChannelHealth {
  id: string;
  name: string;
  page: string;
  base: string;
  enabled: boolean;
  status: ChannelStatus;
  score: number;
  trafficSource: "logs" | "usage" | "none";
  note: string | null;
  metrics: ChannelMetrics;
  scoreTrend: Array<{ at: number; score: number }>;
  alerts: ChannelAlert[];
}

export interface ChannelHealthOverview {
  generatedAt: number;
  windowHours: number;
  summary: {
    channels: number;
    ok: number;
    warning: number;
    critical: number;
    totalRequests: number;
    okRequests: number;
    errRequests: number;
    avgSuccessRate: number | null;
    totalAccounts: number;
    healthyAccounts: number;
    activeAlerts: number;
  };
  channels: ChannelHealth[];
  alerts: ChannelAlert[];
}

export interface ChannelDetail {
  channel: ChannelHealth;
  trend: Array<{ at: number; score: number; requests: number; successRate: number | null }>;
  buckets: Array<{ ts: number; label: string; requests: number; ok: number; failed: number }>;
  models: Array<{ model: string; calls: number; ok: number; failed: number; successRate: number | null; share: number; avgLatencyMs: number | null }>;
  accounts: Array<{ name: string; status: string; enabled: boolean; healthScore: number | null; cooldown: boolean; lastError: string | null; detail: string | null }>;
  recentLogs: Array<{ ts: number; model: string; ok: boolean; latencyMs: number | null; error: string | null }>;
  checkin: {
    accounts: number;
    todayCheckedIn: number;
    lastResults: Array<{ ts: number; accountId: string; result: string; error: string | null }>;
  };
}
