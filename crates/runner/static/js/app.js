/**
 * Oxydra Web Configurator — Main Application
 *
 * Hash-based SPA router with Alpine.js reactive state.
 */

/** Simple fetch wrapper for the API */
async function api(path) {
  const resp = await fetch(`/api/v1${path}`);
  const json = await resp.json();
  if (!resp.ok || json.error) {
    throw new Error(json.error?.message || `API error: ${resp.status}`);
  }
  return json.data;
}

/** Route map: hash fragment → page name */
const ROUTES = {
  '/': 'dashboard',
  '/config/agent': 'config-agent',
  '/config/runner': 'config-runner',
  '/config/users': 'config-users',
  '/logs': 'logs',
  '/setup': 'setup',
};

function app() {
  return {
    // Router state
    currentPage: 'dashboard',

    // Connection status
    connected: false,

    // Loading flag
    loading: false,

    // Meta info from /api/v1/meta
    meta: {},

    // Onboarding status
    onboarding: { needs_setup: false, checks: {} },

    // Status data
    statusUsers: [],

    // Config data
    agentConfig: null,
    runnerConfig: null,
    userList: [],

    // UI state
    openSections: [],

    async init() {
      // Set up hash router
      window.addEventListener('hashchange', () => this.route());
      this.route();

      // Load meta + onboarding status
      try {
        this.meta = await api('/meta');
        this.connected = true;
      } catch (e) {
        console.error('Failed to load meta:', e);
      }

      try {
        this.onboarding = await api('/onboarding/status');
      } catch (e) {
        console.error('Failed to load onboarding status:', e);
      }

      // Load dashboard data
      await this.loadStatus();
    },

    route() {
      const hash = window.location.hash.slice(1) || '/';
      this.currentPage = ROUTES[hash] || 'dashboard';
      this.onPageChange();
    },

    async onPageChange() {
      this.loading = true;
      try {
        switch (this.currentPage) {
          case 'dashboard':
            await this.loadStatus();
            break;
          case 'config-agent':
            await this.loadAgentConfig();
            break;
          case 'config-runner':
            await this.loadRunnerConfig();
            break;
          case 'config-users':
            await this.loadUsers();
            await this.loadStatus();
            break;
        }
      } catch (e) {
        console.error('Page load error:', e);
      }
      this.loading = false;
    },

    async loadStatus() {
      try {
        const data = await api('/status');
        this.statusUsers = data.users || [];
      } catch (e) {
        console.error('Failed to load status:', e);
      }
    },

    async loadAgentConfig() {
      try {
        this.agentConfig = await api('/config/agent');
      } catch (e) {
        console.error('Failed to load agent config:', e);
      }
    },

    async loadRunnerConfig() {
      try {
        this.runnerConfig = await api('/config/runner');
      } catch (e) {
        console.error('Failed to load runner config:', e);
      }
    },

    async loadUsers() {
      try {
        const data = await api('/config/users');
        this.userList = data.users || [];
      } catch (e) {
        console.error('Failed to load users:', e);
      }
    },

    findUserStatus(userId) {
      return this.statusUsers.find(u => u.user_id === userId);
    },

    toggleSection(key) {
      const idx = this.openSections.indexOf(key);
      if (idx >= 0) {
        this.openSections.splice(idx, 1);
      } else {
        this.openSections.push(key);
      }
    },
  };
}

// Make app() available globally for Alpine.js
window.app = app;
