import { useEffect, useState } from 'react';
import type {
  BotInfo,
  BotModelSelection,
  BotsListResponse,
} from '../../shared/bots';
import type { ModelsResponse } from '../../shared/models';
import { usePageData } from '../hooks/use-page-data';
import { api, errorMessage, post } from '../lib/api';
import { navigate } from '../lib/navigation';
import { Bot, ChevronDown, PenLine, Plus, Settings, Trash2, Users } from './icons';
import {
  Button,
  Checkbox,
  Dialog,
  DialogContent,
  DialogDescription,
  DialogTitle,
  Field,
  Input,
  Notice,
  Switch,
} from './ui';
import './bot-workspace.css';

const BOT_COLORS = [
  '#e11d48',
  '#2563eb',
  '#059669',
  '#d97706',
  '#7c3aed',
  '#0891b2',
  '#475569',
];

export function notifyBotsRefresh() {
  window.dispatchEvent(new Event('nod:bots-refresh'));
}

export function useBotsData() {
  const result = usePageData<BotsListResponse>('/api/bots');
  const { refresh } = result;
  useEffect(() => {
    const onRefresh = () => void refresh();
    window.addEventListener('nod:bots-refresh', onRefresh);
    return () => window.removeEventListener('nod:bots-refresh', onRefresh);
  }, [refresh]);
  return result;
}

export function BotSidebar({
  botsState,
  busy = false,
  onSelectRoom,
  onNewChat,
  onOpenSetup,
  onOpenCreate,
  onEditBot,
}: {
  botsState: BotsListResponse | null;
  busy?: boolean;
  onSelectRoom(id: string): void;
  onNewChat(id: string): void;
  onOpenSetup(): void;
  onOpenCreate(): void;
  onEditBot(bot: BotInfo): void;
}) {
  const bots = botsState?.bots ?? [];
  const mainBot = bots.find((b) => b.role === 'main') ?? bots[0];
  const secondaryBots = bots.filter((b) => b.role !== 'main');
  const activeGroup = botsState?.activeRoom === 'group';

  return (
    <aside className="bot-sidebar" aria-label="Bots and rooms">
      <div className="bot-sidebar-top">
        <span className="bot-sidebar-brand">Assistants</span>
        <Button
          type="button"
          variant="ghost"
          size="sm"
          className="bot-new-pill"
          disabled={busy}
          onClick={onOpenCreate}
        >
          <Plus size={14} aria-hidden="true" />
          <span>New bot</span>
        </Button>
      </div>

      {botsState && !botsState.setupCompleted && (
        <button
          type="button"
          className="bot-setup-banner"
          disabled={busy}
          onClick={onOpenSetup}
        >
          <span>Finish Main Bot setup</span>
        </button>
      )}

      <div className="bot-sidebar-section">
        <div className="bot-row">
          <button
            type="button"
            className="bot-row-button"
            aria-current={activeGroup ? 'page' : undefined}
            disabled={busy}
            onClick={() => onSelectRoom('group')}
          >
            <span className="bot-avatar is-group">
              <Users size={13} aria-hidden="true" />
            </span>
            <span className="bot-row-meta">
              <span className="bot-row-name" dir="auto">Group Chat</span>
              <span className="bot-row-sub" dir="auto">@mention bots</span>
            </span>
          </button>
          <Button
            type="button"
            variant="ghost"
            size="icon"
            className="bot-row-action"
            title="New group chat"
            aria-label="New group chat"
            disabled={busy}
            onClick={() => onNewChat('group')}
          >
            <PenLine size={14} aria-hidden="true" />
          </Button>
        </div>

        {mainBot && (
          <div className="bot-row">
            <button
              type="button"
              className="bot-row-button"
              aria-current={
                !activeGroup && botsState?.activeBotId === mainBot.id ? 'page' : undefined
              }
              disabled={busy}
              onClick={() => onSelectRoom(mainBot.id)}
            >
              <span
                className="bot-avatar"
                style={{ background: mainBot.avatarColor || '#e11d48' }}
              >
                <Bot size={13} aria-hidden="true" />
              </span>
              <span className="bot-row-meta">
                <span className="bot-row-name" dir="auto">{mainBot.name}</span>
                <span className="bot-row-sub" dir="auto">Main assistant</span>
              </span>
            </button>
            <div className="bot-row-actions">
              <Button
                type="button"
                variant="ghost"
                size="icon"
                className="bot-row-action"
                title={`Configure ${mainBot.name}`}
                aria-label={`Configure ${mainBot.name}`}
                disabled={busy}
                onClick={() => onEditBot(mainBot)}
              >
                <Settings size={14} aria-hidden="true" />
              </Button>
              <Button
                type="button"
                variant="ghost"
                size="icon"
                className="bot-row-action"
                title={`New chat with ${mainBot.name}`}
                aria-label={`New chat with ${mainBot.name}`}
                disabled={busy}
                onClick={() => onNewChat(mainBot.id)}
              >
                <PenLine size={14} aria-hidden="true" />
              </Button>
            </div>
          </div>
        )}
      </div>

      <div className="bot-sidebar-section">
        <div className="bot-sidebar-heading">
          <span>Bots</span>
          {secondaryBots.length > 0 && <span className="bot-count-pill">{secondaryBots.length}</span>}
        </div>
        {secondaryBots.length === 0 ? (
          <p className="bot-sidebar-empty">
            No extra bots yet.
          </p>
        ) : (
          secondaryBots.map((bot) => (
            <div className="bot-row" key={bot.id}>
              <button
                type="button"
                className="bot-row-button"
                aria-current={
                  !activeGroup && botsState?.activeBotId === bot.id ? 'page' : undefined
                }
                disabled={busy}
                onClick={() => onSelectRoom(bot.id)}
              >
                <span
                  className="bot-avatar"
                  style={{ background: bot.avatarColor || '#2563eb' }}
                >
                  {bot.name.slice(0, 1).toUpperCase()}
                </span>
                <span className="bot-row-meta">
                  <span className="bot-row-name" dir="auto">{bot.name}</span>
                  <span className="bot-row-sub" dir="auto">@{bot.handle}</span>
                </span>
              </button>
              <div className="bot-row-actions">
                <Button
                  type="button"
                  variant="ghost"
                  size="icon"
                  className="bot-row-action"
                  title={`Configure ${bot.name}`}
                  aria-label={`Configure ${bot.name}`}
                  disabled={busy}
                  onClick={() => onEditBot(bot)}
                >
                  <Settings size={14} aria-hidden="true" />
                </Button>
                <Button
                  type="button"
                  variant="ghost"
                  size="icon"
                  className="bot-row-action"
                  title={`New chat with ${bot.name}`}
                  aria-label={`New chat with ${bot.name}`}
                  disabled={busy}
                  onClick={() => onNewChat(bot.id)}
                >
                  <PenLine size={14} aria-hidden="true" />
                </Button>
              </div>
            </div>
          ))
        )}
      </div>
    </aside>
  );
}

interface ModelOption {
  key: string;
  selection: BotModelSelection;
  label: string;
  modelName: string;
  connectionId: string;
  connectionLabel: string;
}

function modelKey(sel: { connectionId: string; modelId: string } | null | undefined) {
  return sel ? `${sel.connectionId}::${sel.modelId}` : '';
}

export function BotEditorDialog({
  open,
  mode,
  bot,
  onOpenChange,
  onSaved,
}: {
  open: boolean;
  mode: 'setup-main' | 'create' | 'edit';
  bot: BotInfo | null;
  onOpenChange(open: boolean): void;
  onSaved(): Promise<void>;
}) {
  const { data: models } = usePageData<ModelsResponse>('/api/models');
  const [name, setName] = useState('');
  const [handle, setHandle] = useState('');
  const [description, setDescription] = useState('');
  const [instructions, setInstructions] = useState('');
  const [avatarColor, setAvatarColor] = useState(BOT_COLORS[0]);
  const [modelChoice, setModelChoice] = useState('');
  const [backups, setBackups] = useState<string[]>([]);
  const [backupQuery, setBackupQuery] = useState('');
  const [memSaving, setMemSaving] = useState(true);
  const [memSensitive, setMemSensitive] = useState(false);
  const [saving, setSaving] = useState(false);
  const [error, setError] = useState('');

  const isMain = mode === 'setup-main' || bot?.role === 'main';

  useEffect(() => {
    if (!open) return;
    setError('');
    setBackupQuery('');
    if (mode === 'create') {
      setName('');
      setHandle('');
      setDescription('');
      setInstructions('');
      setAvatarColor(BOT_COLORS[1]);
      setModelChoice('');
      setBackups([]);
      setMemSaving(true);
      setMemSensitive(false);
      return;
    }
    const target = bot;
    setName(target?.name ?? 'Main Bot');
    setHandle(target?.handle ?? 'main');
    setDescription(target?.description ?? '');
    setInstructions(target?.instructions ?? '');
    setAvatarColor(target?.avatarColor || BOT_COLORS[0]);
    setModelChoice(modelKey(target?.model));
    setBackups((target?.backups ?? []).map((b) => modelKey(b)).filter(Boolean));
    setMemSaving(target?.memorySettings?.saving ?? true);
    setMemSensitive(target?.memorySettings?.sensitive ?? false);
  }, [open, mode, bot]);

  const modelOptions: ModelOption[] = [];
  for (const conn of models?.connections ?? []) {
    if (conn.status !== 'connected') continue;
    for (const m of conn.models ?? []) {
      const sel: BotModelSelection = {
        connectionId: conn.id,
        modelId: m.id,
        effort: 'high',
      };
      modelOptions.push({
        key: modelKey(sel),
        selection: sel,
        label: `${conn.label} / ${m.name}`,
        modelName: m.name || m.id,
        connectionId: conn.id,
        connectionLabel: conn.label,
      });
    }
  }

  function findOption(key: string): BotModelSelection | null {
    return modelOptions.find((opt) => opt.key === key)?.selection ?? null;
  }

  async function submit(e: React.FormEvent) {
    e.preventDefault();
    if (!name.trim()) {
      setError('Give this bot a name.');
      return;
    }
    setSaving(true);
    setError('');
    try {
      const payload = {
        name: name.trim(),
        ...(!isMain && handle.trim() ? { handle: handle.trim() } : {}),
        description: description.trim(),
        instructions: instructions.trim(),
        avatarColor,
        model: modelChoice ? findOption(modelChoice) : null,
        backups: backups
          .map(findOption)
          .filter((item): item is BotModelSelection => Boolean(item))
          .slice(0, 3),
        memorySettings: { saving: memSaving, sensitive: memSensitive },
      };

      if (mode === 'setup-main') {
        await post('/api/bots/setup', payload);
      } else if (mode === 'create') {
        const res = await post<{ bot?: BotInfo }>('/api/bots', payload);
        if (res.bot?.id) {
          await post(`/api/bots/${encodeURIComponent(res.bot.id)}/select`, {});
        }
      } else if (bot) {
        await api(`/api/bots/${encodeURIComponent(bot.id)}`, {
          method: 'PATCH',
          body: JSON.stringify(payload),
        });
      }
      await onSaved();
      notifyBotsRefresh();
      onOpenChange(false);
    } catch (err) {
      setError(errorMessage(err));
    } finally {
      setSaving(false);
    }
  }

  async function removeBot() {
    if (!bot || isMain) return;
    setSaving(true);
    setError('');
    try {
      await api(`/api/bots/${encodeURIComponent(bot.id)}`, { method: 'DELETE' });
      await onSaved();
      notifyBotsRefresh();
      onOpenChange(false);
    } catch (err) {
      setError(errorMessage(err));
    } finally {
      setSaving(false);
    }
  }

  function toggleBackup(key: string) {
    setBackups((prev) =>
      prev.includes(key)
        ? prev.filter((item) => item !== key)
        : prev.length < 3
          ? [...prev, key]
          : prev,
    );
  }

  const title =
    mode === 'setup-main'
      ? 'Set up Main Bot'
      : mode === 'create'
        ? 'Create bot'
        : bot?.name || 'Edit bot';
  const subtitle =
    mode === 'setup-main'
      ? 'Choose a name and default model for your primary assistant.'
      : mode === 'create'
        ? 'Give this assistant a focused role and its own model.'
        : 'Update identity, instructions, and model routing.';

  const previewLetter = (name.trim() || (isMain ? 'M' : 'B')).slice(0, 1).toUpperCase();
  const availableBackups = modelOptions.filter((opt) => opt.key !== modelChoice);
  const terms = backupQuery.trim().toLowerCase().split(/\s+/).filter(Boolean);
  const filteredBackups = terms.length
    ? availableBackups.filter((opt) =>
        `${opt.connectionLabel} ${opt.modelName} ${opt.selection.modelId}`
          .toLowerCase()
          .includes(terms.join(' ')) ||
        terms.every((t) =>
          `${opt.connectionLabel} ${opt.modelName} ${opt.selection.modelId}`
            .toLowerCase()
            .includes(t),
        ),
      )
    : availableBackups;

  const groupedBackups: Array<{ connectionId: string; label: string; items: ModelOption[] }> = [];
  for (const opt of filteredBackups) {
    let group = groupedBackups.find((g) => g.connectionId === opt.connectionId);
    if (!group) {
      group = { connectionId: opt.connectionId, label: opt.connectionLabel, items: [] };
      groupedBackups.push(group);
    }
    group.items.push(opt);
  }

  return (
    <Dialog open={open} onOpenChange={onOpenChange}>
      <DialogContent className="bot-editor-dialog">
        <div className="bot-dialog-header">
          <div className="bot-dialog-badge" style={{ background: avatarColor }} aria-hidden="true">
            {isMain ? <Bot size={20} /> : <span>{previewLetter}</span>}
          </div>
          <div className="bot-dialog-titles">
            <DialogTitle>{title}</DialogTitle>
            <DialogDescription>{subtitle}</DialogDescription>
          </div>
        </div>

        <form onSubmit={(e) => void submit(e)} className="bot-dialog-body">
          <div className="bot-dialog-scroll">
            {error && <Notice>{error}</Notice>}

            <div className="bot-identity-row">
              <Field label="Name">
                <Input
                  value={name}
                  placeholder={isMain ? 'Main Bot' : 'e.g. Research Bot'}
                  autoFocus
                  onChange={(e) => setName(e.target.value)}
                />
              </Field>
              <div className="field">
                <span className="field-label">Color</span>
                <div className="bot-color-row" role="group" aria-label="Avatar color">
                  {BOT_COLORS.map((color) => (
                    <button
                      key={color}
                      type="button"
                      className="bot-color-dot"
                      style={{ background: color }}
                      aria-label={`Color ${color}`}
                      aria-pressed={avatarColor === color}
                      onClick={() => setAvatarColor(color)}
                    />
                  ))}
                </div>
              </div>
            </div>

            <div className={isMain ? 'bot-form-single' : 'bot-form-pair'}>
              {!isMain && (
                <Field label="Mention handle">
                  <Input
                    value={handle}
                    placeholder="@auto-from-name"
                    onChange={(e) => setHandle(e.target.value)}
                  />
                </Field>
              )}
              <Field label="Focus">
                <Input
                  value={description}
                  placeholder={isMain ? 'General assistant' : 'Code review, research…'}
                  onChange={(e) => setDescription(e.target.value)}
                />
              </Field>
            </div>

            <Field label="Model">
              <div className="bot-select-wrap">
                <select
                  className="input bot-select"
                  value={modelChoice}
                  onChange={(e) => setModelChoice(e.target.value)}
                >
                  <option value="">Use global default model</option>
                  {modelOptions.map((opt) => (
                    <option key={opt.key} value={opt.key}>
                      {opt.label}
                    </option>
                  ))}
                </select>
                <ChevronDown size={14} className="bot-select-chevron" aria-hidden="true" />
              </div>
            </Field>

            <Field label="Instructions">
              <textarea
                className="input bot-textarea"
                rows={3}
                value={instructions}
                placeholder="Optional system prompt or response style…"
                onChange={(e) => setInstructions(e.target.value)}
              />
            </Field>

            <details className="bot-advanced">
              <summary className="bot-advanced-summary">
                <span>Memory & fallback models</span>
                <span className="bot-advanced-meta">
                  {backups.length > 0 ? `${backups.length}/3 backup` : !memSaving ? 'Memory off' : 'Optional'}
                  <ChevronDown size={14} className="bot-advanced-chevron" aria-hidden="true" />
                </span>
              </summary>

              <div className="bot-advanced-body">
                <div className="bot-toggle-list">
                  <label className="bot-toggle-row">
                    <span>Save dedicated memory notes</span>
                    <Switch checked={memSaving} onCheckedChange={setMemSaving} />
                  </label>
                  <label className="bot-toggle-row">
                    <span>Allow sensitive details</span>
                    <Switch checked={memSensitive} onCheckedChange={setMemSensitive} />
                  </label>
                </div>

                {bot && (
                  <button
                    type="button"
                    className="bot-memory-link"
                    onClick={() => {
                      onOpenChange(false);
                      window.dispatchEvent(
                        new CustomEvent('nod:select-memory-bot', { detail: { botId: bot.id } }),
                      );
                      navigate('/settings/memory');
                    }}
                  >
                    View {bot.name} memory ({bot.memoryCount ?? 0} notes) →
                  </button>
                )}

                {availableBackups.length > 0 && (
                  <div className="bot-backups-block">
                    <div className="bot-backups-header">
                      <span className="field-label">Backup models (up to 3)</span>
                      <span className="bot-advanced-meta">{backups.length}/3</span>
                    </div>
                    {availableBackups.length > 6 && (
                      <Input
                        value={backupQuery}
                        placeholder="Search models or providers…"
                        className="bot-backup-search"
                        onChange={(e) => setBackupQuery(e.target.value)}
                      />
                    )}
                    <div className="bot-backup-list">
                      {groupedBackups.length === 0 ? (
                        <p className="bot-backup-empty">No matching models.</p>
                      ) : (
                        groupedBackups.map((group) => {
                          const hasSelected = group.items.some((item) => backups.includes(item.key));
                          const shouldOpen =
                            groupedBackups.length <= 2 || Boolean(terms.length) || hasSelected;
                          return (
                            <details
                              key={group.connectionId}
                              className="bot-backup-group"
                              open={shouldOpen}
                            >
                              <summary className="bot-backup-group-title">
                                <span>{group.label}</span>
                                <span className="bot-backup-group-count">{group.items.length}</span>
                              </summary>
                              <div className="bot-backup-group-items">
                                {group.items.map((opt) => (
                                  <label key={opt.key} className="bot-backup-item">
                                    <Checkbox
                                      checked={backups.includes(opt.key)}
                                      disabled={!backups.includes(opt.key) && backups.length >= 3}
                                      onChange={() => toggleBackup(opt.key)}
                                    />
                                    <span className="bot-backup-label">{opt.modelName}</span>
                                  </label>
                                ))}
                              </div>
                            </details>
                          );
                        })
                      )}
                    </div>
                  </div>
                )}
              </div>
            </details>
          </div>

          <div className="bot-dialog-actions">
            <div>
              {mode === 'edit' && !isMain && (
                <Button
                  type="button"
                  variant="ghost"
                  size="sm"
                  className="bot-delete-btn"
                  disabled={saving}
                  onClick={() => void removeBot()}
                >
                  <Trash2 size={14} aria-hidden="true" />
                  <span>Delete</span>
                </Button>
              )}
            </div>
            <div className="bot-dialog-buttons">
              <Button
                type="button"
                variant="ghost"
                disabled={saving}
                onClick={() => onOpenChange(false)}
              >
                {mode === 'setup-main' ? 'Skip' : 'Cancel'}
              </Button>
              <Button type="submit" disabled={saving}>
                {saving
                  ? 'Saving…'
                  : mode === 'setup-main'
                    ? 'Save Main Bot'
                    : mode === 'create'
                      ? 'Create bot'
                      : 'Save changes'}
              </Button>
            </div>
          </div>
        </form>
      </DialogContent>
    </Dialog>
  );
}
