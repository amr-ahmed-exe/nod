export interface BotModelSelection {
  connectionId: string;
  modelId: string;
  effort: 'low' | 'medium' | 'high' | 'max';
}

export interface BotMemorySettings {
  saving: boolean;
  sensitive: boolean;
}

export interface BotInfo {
  id: string;
  name: string;
  handle: string;
  role: 'main' | 'secondary';
  description: string;
  instructions: string;
  avatarColor: string;
  model: BotModelSelection | null;
  backups: BotModelSelection[];
  memorySettings: BotMemorySettings;
  threadId: string;
  memoryCount?: number;
  messageCount?: number;
  createdAt: string;
  updatedAt: string;
}

export interface BotsListResponse {
  mainBotId: string;
  activeBotId: string;
  activeRoom: '1on1' | 'group' | string;
  groupThreadId: string;
  setupCompleted: boolean;
  bots: BotInfo[];
}

export interface BotVisibleSummary {
  id: string;
  name: string;
  handle: string;
  description: string;
  memories: Array<{
    path?: string;
    area?: string;
    text?: string;
    updated?: string;
  }>;
  recentMessages: string[];
}

export interface BotContextSnapshot {
  botId: string;
  role: 'main' | 'secondary';
  ownMemories: unknown[];
  visibleBots: BotVisibleSummary[];
}
