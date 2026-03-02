(function initLogsModule() {
  function createLogsState() {
    return {
      userId: '',
      role: 'runtime',
      stream: 'both',
      tail: 200,
      format: 'json',
      autoRefresh: true,
      entries: [],
      warnings: [],
      truncated: false,
      loading: false,
    };
  }

  function inferLevel(entry) {
    const message = String(entry?.message || '').toLowerCase();
    if (message.includes('error') || message.includes('panic') || message.includes('fatal')) {
      return 'error';
    }
    if (message.includes('warn')) {
      return 'warn';
    }
    if (message.includes('debug')) {
      return 'debug';
    }
    if (message.includes('trace')) {
      return 'trace';
    }
    return 'info';
  }

  function toTextLine(entry) {
    const timestamp = entry?.timestamp || '-';
    const source = entry?.source || 'process_file';
    const role = entry?.role || 'unknown';
    const stream = entry?.stream || 'stdout';
    const message = entry?.message || '';
    return `${timestamp} [${source}][${role}][${stream}] ${message}`;
  }

  window.OxydraLogs = {
    createLogsState,
    inferLevel,
    toTextLine,
  };
}());
