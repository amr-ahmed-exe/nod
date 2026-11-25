import { useEffect, useState } from 'react';
import type { PageView } from '../../shared/pages';
import { usePageData } from '../hooks/use-page-data';
import { errorMessage, post } from '../lib/api';
import { subscribeAppEvents } from '../lib/app-events';
import { ownedArtifactUrl } from '../lib/attachment-url';
import { navigate } from '../lib/navigation';
import type { ImagePreview } from './attachment-images';
import { artifactFromPreview } from './file-actions';
import { LayoutDashboard } from './icons';
import { Button } from './ui';

export function KeepInPages({ item }: { item: ImagePreview }) {
  const [busy, setBusy] = useState(false);
  const [page, setPage] = useState<PageView | null>(null);
  const { data: keptPages, refresh } = usePageData<PageView[]>('/api/pages');
  useEffect(
    () =>
      subscribeAppEvents(undefined, {
        open() {},
        error() {},
        message(event) {
          try {
            const payload = JSON.parse(event.data) as { type?: string };
            if (payload.type === 'pages') void refresh();
          } catch {}
        },
      }),
    [refresh],
  );
  const artifact = artifactFromPreview(item);
  if (!ownedArtifactUrl(artifact.url)) return null;
  const resolved =
    page ?? keptPages?.find((entry) => !entry.archivedAt && entry.file?.id === artifact.id) ?? null;
  if (resolved)
    return (
      <Button variant="ghost" size="sm" onClick={() => navigate(`/pages?page=${resolved.id}`)}>
        Open in Pages
      </Button>
    );
  return (
    <Button
      variant="ghost"
      size="icon"
      disabled={busy}
      aria-label={`Keep ${item.name} in Pages`}
      title="Keep in Pages"
      onClick={async () => {
        setBusy(true);
        try {
          setPage(await post<PageView>('/api/pages', { artifactId: artifact.id }));
          void refresh();
        } catch (cause) {
          window.dispatchEvent(
            new CustomEvent('nod:file-notice', {
              detail: `Nod couldn't keep ${item.name} in Pages: ${errorMessage(cause)}`,
            }),
          );
        } finally {
          setBusy(false);
        }
      }}
    >
      <LayoutDashboard size={16} />
    </Button>
  );
}
