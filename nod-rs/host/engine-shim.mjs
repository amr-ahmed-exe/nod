import { pathToFileURL } from 'node:url';

const listeners = new Set();
process.on('message', (data) => {
  for (const listener of [...listeners]) listener({ data });
});
process.parentPort = {
  on(event, listener) {
    if (event === 'message') listeners.add(listener);
  },
  once(event, listener) {
    if (event !== 'message') return;
    const once = (value) => {
      listeners.delete(once);
      listener(value);
    };
    listeners.add(once);
  },
  removeListener(event, listener) {
    listeners.delete(listener);
  },
  postMessage(message) {
    process.send?.(message);
  },
};

await import(pathToFileURL(process.env.NOD_ENGINE_ENTRY).href);
