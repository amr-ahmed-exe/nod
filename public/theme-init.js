(() => {
  let theme;
  try {
    theme = localStorage.getItem('nod.theme');
  } catch {}
  if (theme !== 'light' && theme !== 'dark' && theme !== 'amoled' && theme !== 'catppuccin-mocha')
    theme = window.matchMedia?.('(prefers-color-scheme: dark)').matches ? 'dark' : 'light';
  document.documentElement.dataset.theme = theme;
  document.documentElement.style.colorScheme = theme === 'light' ? 'light' : 'dark';
  const metaColor =
    theme === 'amoled'
      ? '#000000'
      : theme === 'catppuccin-mocha'
        ? '#1e1e2e'
        : theme === 'dark'
          ? '#1e1b21'
          : '#fdfcfe';
  document.querySelector('meta[name="theme-color"]')?.setAttribute('content', metaColor);
})();
