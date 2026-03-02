const SECRET_MASK = '********';
const SECRET_SENTINEL = '__UNCHANGED__';

const ROUTES = {
  '/': 'dashboard',
  '/config/agent': 'config-agent',
  '/config/runner': 'config-runner',
  '/config/users': 'config-users',
  '/control': 'control',
  '/logs': 'logs',
  '/setup': 'setup',
};

async function api(path, options = {}) {
  const method = (options.method || 'GET').toUpperCase();
  const headers = {
    Accept: 'application/json',
    ...(options.headers || {}),
  };
  const request = {
    method,
    headers,
  };

  if (options.body !== undefined) {
    headers['Content-Type'] = 'application/json';
    request.body = JSON.stringify(options.body);
  }

  const response = await fetch(`/api/v1${path}`, request);
  const payload = await response.json().catch(() => ({}));
  if (!response.ok || payload.error) {
    const message = payload.error?.message || `API request failed with status ${response.status}`;
    const error = new Error(message);
    error.code = payload.error?.code;
    error.status = response.status;
    throw error;
  }

  return payload.data;
}

function deepClone(value) {
  return JSON.parse(JSON.stringify(value));
}

function deepEqual(left, right) {
  return JSON.stringify(left) === JSON.stringify(right);
}

function isObject(value) {
  return value !== null && typeof value === 'object' && !Array.isArray(value);
}

function inferFieldType(value) {
  if (value === SECRET_MASK) {
    return 'secret';
  }
  if (typeof value === 'boolean') {
    return 'boolean';
  }
  if (typeof value === 'number') {
    return 'number';
  }
  if (typeof value === 'string') {
    return 'string';
  }
  return 'json';
}

function createField(path, rawValue) {
  const type = inferFieldType(rawValue);
  if (type === 'secret') {
    return {
      path,
      type,
      value: '',
      original: SECRET_MASK,
      changed: false,
      touched: false,
      parseError: '',
    };
  }

  if (type === 'json') {
    return {
      path,
      type,
      value: JSON.stringify(rawValue, null, 2),
      original: deepClone(rawValue),
      changed: false,
      touched: false,
      parseError: '',
    };
  }

  return {
    path,
    type,
    value: rawValue,
    original: deepClone(rawValue),
    changed: false,
    touched: false,
    parseError: '',
  };
}

function buildFields(config, currentPath = '', fields = []) {
  if (Array.isArray(config)) {
    fields.push(createField(currentPath, config));
    return fields;
  }

  if (!isObject(config)) {
    fields.push(createField(currentPath, config));
    return fields;
  }

  const keys = Object.keys(config).sort();
  if (keys.length === 0 && currentPath) {
    fields.push(createField(currentPath, config));
    return fields;
  }

  keys.forEach((key) => {
    const nextPath = currentPath ? `${currentPath}.${key}` : key;
    const value = config[key];
    if (isObject(value)) {
      buildFields(value, nextPath, fields);
      return;
    }
    if (Array.isArray(value)) {
      fields.push(createField(nextPath, value));
      return;
    }
    fields.push(createField(nextPath, value));
  });

  return fields;
}

function setPatchPath(target, path, value) {
  const keys = path.split('.');
  let current = target;
  for (let idx = 0; idx < keys.length - 1; idx += 1) {
    const segment = keys[idx];
    if (!isObject(current[segment])) {
      current[segment] = {};
    }
    current = current[segment];
  }
  current[keys[keys.length - 1]] = value;
}

function parseFieldValue(field) {
  if (field.type === 'secret') {
    if (!field.touched) {
      return SECRET_SENTINEL;
    }
    return field.value;
  }
  if (field.type === 'boolean') {
    return Boolean(field.value);
  }
  if (field.type === 'number') {
    if (field.value === '' || field.value === null) {
      throw new Error(`Field ${field.path} must be a number`);
    }
    const parsed = Number(field.value);
    if (!Number.isFinite(parsed)) {
      throw new Error(`Field ${field.path} must be a finite number`);
    }
    return parsed;
  }
  if (field.type === 'json') {
    return JSON.parse(field.value);
  }
  return field.value;
}

function fieldHasChanges(field) {
  if (field.type === 'secret') {
    return field.touched;
  }
  try {
    const currentValue = parseFieldValue(field);
    return !deepEqual(currentValue, field.original);
  } catch {
    return true;
  }
}

function createEditor(response, endpoint) {
  return {
    endpoint,
    fileExists: response.file_exists,
    filePath: response.file_path,
    fields: buildFields(response.config),
    changedFields: [],
    backupPath: '',
    restartRequired: false,
  };
}

function onboardingFactory() {
  if (window.OxydraOnboarding?.createOnboardingState) {
    return window.OxydraOnboarding.createOnboardingState();
  }
  return {
    step: 1,
    runnerWorkspaceRoot: '.oxydra/workspaces',
    userId: 'default',
    userConfigPath: 'users/default.toml',
    providerName: 'openai',
    providerType: 'openai',
    providerApiKey: '',
    providerApiKeyEnv: 'OPENAI_API_KEY',
    providerEnvResolved: false,
    busy: false,
    error: '',
    done: false,
  };
}

function app() {
  return {
    currentPage: 'dashboard',
    connected: false,
    loading: false,
    saving: false,
    meta: {},
    onboarding: { needs_setup: false, checks: {} },
    statusUsers: [],
    userList: [],
    runnerEditor: null,
    agentEditor: null,
    userEditor: null,
    selectedUserId: '',
    restartNotice: '',
    toast: {
      visible: false,
      message: '',
      kind: 'info',
    },
    newUser: {
      user_id: '',
      config_path: '',
    },
    onboardingWizard: onboardingFactory(),
    controlState: window.OxydraControl?.createControlState
      ? window.OxydraControl.createControlState()
      : { busyByUser: {} },
    logsState: window.OxydraLogs?.createLogsState
      ? window.OxydraLogs.createLogsState()
      : {
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
      },

    async init() {
      window.addEventListener('hashchange', () => this.route());
      this._controlPollTimer = window.setInterval(() => {
        if (document.visibilityState !== 'visible' || this.currentPage !== 'control') {
          return;
        }
        this.loadStatus().catch((error) => this.showToast(error.message, 'error'));
      }, 5000);
      this._logsPollTimer = window.setInterval(() => {
        if (
          document.visibilityState !== 'visible'
          || this.currentPage !== 'logs'
          || !this.logsState.autoRefresh
          || !this.logsState.userId
        ) {
          return;
        }
        this.refreshLogs().catch((error) => this.showToast(error.message, 'error'));
      }, 2000);
      await this.refreshCoreData();
      this.route();
    },

    async refreshCoreData() {
      try {
        this.meta = await api('/meta');
        this.connected = true;
      } catch (error) {
        this.connected = false;
        this.showToast(error.message, 'error');
      }

      await this.refreshOnboardingStatus();
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
        if (this.currentPage === 'dashboard') {
          await this.loadStatus();
        } else if (this.currentPage === 'config-agent') {
          await this.loadAgentConfig();
        } else if (this.currentPage === 'config-runner') {
          await this.loadRunnerConfig();
        } else if (this.currentPage === 'config-users') {
          await this.loadUsersPage();
        } else if (this.currentPage === 'control') {
          await this.loadControlPage();
        } else if (this.currentPage === 'logs') {
          await this.loadLogsPage();
        } else if (this.currentPage === 'setup') {
          this.seedOnboardingWizard();
        }
      } catch (error) {
        this.showToast(error.message, 'error');
      } finally {
        this.loading = false;
      }
    },

    async refreshOnboardingStatus() {
      try {
        this.onboarding = await api('/onboarding/status');
      } catch (error) {
        this.showToast(error.message, 'error');
      }
    },

    async loadStatus() {
      const data = await api('/status');
      this.statusUsers = data.users || [];
    },

    async loadRunnerConfig() {
      const response = await api('/config/runner');
      this.runnerEditor = createEditor(response, '/config/runner');
      const workspaceField = this.runnerEditor.fields.find((field) => field.path === 'workspace_root');
      if (workspaceField) {
        this.onboardingWizard.runnerWorkspaceRoot = workspaceField.value;
      }
    },

    async loadAgentConfig() {
      const response = await api('/config/agent');
      this.agentEditor = createEditor(response, '/config/agent');
    },

    async loadUsersPage() {
      await this.loadUsers();
      await this.loadStatus();
      if (this.selectedUserId) {
        const exists = this.userList.some((user) => user.user_id === this.selectedUserId);
        if (exists) {
          await this.loadUserConfig(this.selectedUserId);
          return;
        }
      }

      if (this.userList.length > 0) {
        await this.loadUserConfig(this.userList[0].user_id);
      } else {
        this.selectedUserId = '';
        this.userEditor = null;
      }
    },

    async loadUsers() {
      const data = await api('/config/users');
      this.userList = data.users || [];
    },

    async loadUserConfig(userId) {
      this.selectedUserId = userId;
      const encodedUser = encodeURIComponent(userId);
      const response = await api(`/config/users/${encodedUser}`);
      this.userEditor = createEditor(response, `/config/users/${encodedUser}`);
    },

    findUserStatus(userId) {
      return this.statusUsers.find((entry) => entry.user_id === userId);
    },

    isUserRunning(userId) {
      const status = this.findUserStatus(userId);
      return Boolean(status && status.daemon_running);
    },

    controlUserBusy(userId) {
      if (window.OxydraControl?.isUserBusy) {
        return window.OxydraControl.isUserBusy(this.controlState, userId);
      }
      return Boolean(this.controlState.busyByUser[userId]);
    },

    setControlUserBusy(userId, busy) {
      if (window.OxydraControl?.setUserBusy) {
        window.OxydraControl.setUserBusy(this.controlState, userId, busy);
        return;
      }
      this.controlState.busyByUser[userId] = Boolean(busy);
    },

    async loadControlPage() {
      await this.loadUsers();
      await this.loadStatus();
    },

    async runControlAction(userId, action) {
      if (!userId || this.controlUserBusy(userId)) {
        return;
      }
      this.setControlUserBusy(userId, true);
      try {
        await api(`/control/${encodeURIComponent(userId)}/${action}`, {
          method: 'POST',
          body: {},
        });
        await this.loadStatus();
        this.showToast(`User ${userId}: ${action} completed.`, 'success');
      } catch (error) {
        this.showToast(error.message, 'error');
      } finally {
        this.setControlUserBusy(userId, false);
      }
    },

    async loadLogsPage() {
      await this.loadUsers();
      await this.loadStatus();
      const hasSelectedUser = this.userList.some((user) => user.user_id === this.logsState.userId);
      if (!hasSelectedUser) {
        this.logsState.userId = this.userList[0]?.user_id || '';
      }
      if (!this.logsState.userId) {
        this.logsState.entries = [];
        this.logsState.warnings = [];
        this.logsState.truncated = false;
        return;
      }
      await this.refreshLogs();
    },

    async setLogsUser(userId) {
      this.logsState.userId = userId;
      await this.refreshLogs();
    },

    logEntryLevel(entry) {
      if (window.OxydraLogs?.inferLevel) {
        return window.OxydraLogs.inferLevel(entry);
      }
      const message = String(entry?.message || '').toLowerCase();
      if (message.includes('error')) return 'error';
      if (message.includes('warn')) return 'warn';
      if (message.includes('debug')) return 'debug';
      if (message.includes('trace')) return 'trace';
      return 'info';
    },

    logEntryText(entry) {
      if (window.OxydraLogs?.toTextLine) {
        return window.OxydraLogs.toTextLine(entry);
      }
      const ts = entry?.timestamp || '-';
      return `${ts} [${entry.source || 'process_file'}][${entry.role}][${entry.stream}] ${entry.message || ''}`;
    },

    decorateLogEntries(entries) {
      return (entries || []).map((entry) => ({
        ...entry,
        _level: this.logEntryLevel(entry),
        _text: this.logEntryText(entry),
      }));
    },

    async refreshLogs() {
      if (!this.logsState.userId) {
        return;
      }
      this.logsState.loading = true;
      try {
        const params = new URLSearchParams({
          role: this.logsState.role,
          stream: this.logsState.stream,
          tail: String(this.logsState.tail || 200),
          format: this.logsState.format || 'json',
        });
        const data = await api(`/logs/${encodeURIComponent(this.logsState.userId)}?${params.toString()}`);
        this.logsState.entries = this.decorateLogEntries(data.entries);
        this.logsState.warnings = data.warnings || [];
        this.logsState.truncated = Boolean(data.truncated);
      } catch (error) {
        this.showToast(error.message, 'error');
      } finally {
        this.logsState.loading = false;
      }
    },

    async copyLogsToClipboard() {
      const text = this.logsState.entries
        .map((entry) => entry._text || this.logEntryText(entry))
        .join('\n');
      if (!text) {
        this.showToast('No log lines to copy.', 'info');
        return;
      }
      try {
        await navigator.clipboard.writeText(text);
        this.showToast('Copied logs to clipboard.', 'success');
      } catch {
        this.showToast('Clipboard copy failed.', 'error');
      }
    },

    updateTextField(editor, field, nextValue) {
      field.value = nextValue;
      field.touched = true;
      field.changed = fieldHasChanges(field);
      field.parseError = '';
      if (editor) {
        editor.changedFields = [];
      }
    },

    updateBooleanField(editor, field, nextValue) {
      field.value = nextValue;
      field.touched = true;
      field.changed = fieldHasChanges(field);
      field.parseError = '';
      if (editor) {
        editor.changedFields = [];
      }
    },

    updateNumberField(editor, field, nextValue) {
      field.value = nextValue;
      field.touched = true;
      field.changed = fieldHasChanges(field);
      field.parseError = '';
      if (editor) {
        editor.changedFields = [];
      }
    },

    updateJsonField(editor, field, nextValue) {
      field.value = nextValue;
      field.touched = true;
      field.changed = fieldHasChanges(field);
      field.parseError = '';
      if (editor) {
        editor.changedFields = [];
      }
    },

    clearSecretField(editor, field) {
      field.value = '';
      field.touched = true;
      field.changed = true;
      if (editor) {
        editor.changedFields = [];
      }
    },

    editorHasChanges(editor) {
      return Boolean(editor && editor.fields.some((field) => fieldHasChanges(field)));
    },

    buildEditorPatch(editor) {
      const patch = {};
      let hasUserChanges = false;

      editor.fields.forEach((field) => {
        if (field.type === 'secret') {
          const value = field.touched ? field.value : SECRET_SENTINEL;
          setPatchPath(patch, field.path, value);
          if (field.touched) {
            hasUserChanges = true;
          }
          return;
        }

        if (!field.changed) {
          return;
        }

        try {
          const parsed = parseFieldValue(field);
          setPatchPath(patch, field.path, parsed);
          field.parseError = '';
          hasUserChanges = true;
        } catch (error) {
          field.parseError = error.message;
          throw error;
        }
      });

      return { patch, hasUserChanges };
    },

    async saveEditor(editor, label, reloadCallback) {
      if (!editor || this.saving) {
        return;
      }

      let patchPayload;
      try {
        patchPayload = this.buildEditorPatch(editor);
      } catch (error) {
        this.showToast(error.message, 'error');
        return;
      }

      if (!patchPayload.hasUserChanges) {
        this.showToast(`No changes to save for ${label}.`, 'info');
        return;
      }

      this.saving = true;
      try {
        const validation = await api(`${editor.endpoint}/validate`, {
          method: 'POST',
          body: patchPayload.patch,
        });
        const changedFields = validation.changed_fields || [];
        editor.changedFields = changedFields;

        if (changedFields.length === 0) {
          this.showToast(`No effective changes for ${label}.`, 'info');
          return;
        }

        const preview = changedFields.slice(0, 12).join('\n');
        const accepted = window.confirm(
          `Save ${label}?\n\nChanged fields:\n${preview}${changedFields.length > 12 ? '\n...' : ''}`
        );
        if (!accepted) {
          return;
        }

        const result = await api(editor.endpoint, {
          method: 'PATCH',
          body: patchPayload.patch,
        });

        editor.changedFields = result.changed_fields || [];
        editor.backupPath = result.backup_path || '';
        editor.restartRequired = Boolean(result.restart_required);
        if (editor.restartRequired) {
          this.restartNotice = 'Configuration was saved. Restart is required for running daemon(s).';
        }

        await reloadCallback();
        await this.loadStatus();
        await this.refreshOnboardingStatus();

        this.showToast(`${label} saved successfully.`, 'success');
      } catch (error) {
        this.showToast(error.message, 'error');
      } finally {
        this.saving = false;
      }
    },

    async saveRunnerConfig() {
      await this.saveEditor(this.runnerEditor, 'Runner configuration', async () => {
        await this.loadRunnerConfig();
      });
    },

    async saveAgentConfig() {
      await this.saveEditor(this.agentEditor, 'Agent configuration', async () => {
        await this.loadAgentConfig();
      });
    },

    async saveUserConfig() {
      if (!this.selectedUserId) {
        return;
      }
      await this.saveEditor(this.userEditor, `User ${this.selectedUserId} configuration`, async () => {
        await this.loadUserConfig(this.selectedUserId);
      });
    },

    async addUser() {
      if (!this.newUser.user_id.trim()) {
        this.showToast('User ID is required.', 'error');
        return;
      }
      if (!this.newUser.config_path.trim()) {
        this.newUser.config_path = this.defaultUserConfigPath(this.newUser.user_id);
      }

      this.saving = true;
      try {
        await api('/config/users', {
          method: 'POST',
          body: {
            user_id: this.newUser.user_id.trim(),
            config_path: this.newUser.config_path.trim(),
          },
        });

        this.showToast(`User ${this.newUser.user_id.trim()} was added.`, 'success');
        this.newUser.user_id = '';
        this.newUser.config_path = '';
        await this.loadUsersPage();
        await this.refreshOnboardingStatus();
      } catch (error) {
        this.showToast(error.message, 'error');
      } finally {
        this.saving = false;
      }
    },

    async deleteUser(userId) {
      const confirmed = window.confirm(`Delete user ${userId}? This also deletes the user config file.`);
      if (!confirmed) {
        return;
      }

      this.saving = true;
      try {
        await api(`/config/users/${encodeURIComponent(userId)}?delete_config_file=true`, {
          method: 'DELETE',
          body: {},
        });
        this.showToast(`User ${userId} was removed.`, 'success');
        await this.loadUsersPage();
        await this.refreshOnboardingStatus();
      } catch (error) {
        this.showToast(error.message, 'error');
      } finally {
        this.saving = false;
      }
    },

    defaultUserConfigPath(userId) {
      if (window.OxydraOnboarding?.inferDefaultUserConfigPath) {
        return window.OxydraOnboarding.inferDefaultUserConfigPath(userId);
      }
      const trimmed = (userId || '').trim();
      return trimmed ? `users/${trimmed}.toml` : 'users/default.toml';
    },

    seedOnboardingWizard() {
      if (this.runnerEditor) {
        const workspaceField = this.runnerEditor.fields.find((field) => field.path === 'workspace_root');
        if (workspaceField) {
          this.onboardingWizard.runnerWorkspaceRoot = workspaceField.value;
        }
      }
      if (!this.onboardingWizard.userConfigPath.trim()) {
        this.onboardingWizard.userConfigPath = this.defaultUserConfigPath(this.onboardingWizard.userId);
      }
    },

    onboardingStepLabel() {
      if (window.OxydraOnboarding?.stepLabel) {
        return window.OxydraOnboarding.stepLabel(this.onboardingWizard.step);
      }
      return `Step ${this.onboardingWizard.step}`;
    },

    onboardingSummary() {
      return [
        `Workspace root: ${this.onboardingWizard.runnerWorkspaceRoot || '(not set)'}`,
        `User: ${this.onboardingWizard.userId || '(not set)'}`,
        `User config path: ${this.onboardingWizard.userConfigPath || '(not set)'}`,
        `Provider: ${this.onboardingWizard.providerName || '(not set)'}`,
        `Provider type: ${this.onboardingWizard.providerType || '(not set)'}`,
        this.onboardingWizard.providerApiKey
          ? 'Provider credential: inline API key'
          : `Provider credential env: ${this.onboardingWizard.providerApiKeyEnv || '(not set)'}`,
      ];
    },

    onboardingAutofillUserPath() {
      this.onboardingWizard.userConfigPath = this.defaultUserConfigPath(this.onboardingWizard.userId);
    },

    onboardingPreviousStep() {
      if (this.onboardingWizard.step > 1) {
        this.onboardingWizard.step -= 1;
      }
    },

    async onboardingNextStep() {
      if (this.onboardingWizard.busy) {
        return;
      }

      this.onboardingWizard.error = '';

      try {
        this.onboardingWizard.busy = true;

        if (this.onboardingWizard.step === 1) {
          this.onboardingWizard.step = 2;
          return;
        }

        if (this.onboardingWizard.step === 2) {
          if (!this.onboardingWizard.runnerWorkspaceRoot.trim()) {
            throw new Error('Workspace root is required.');
          }

          const patch = {
            workspace_root: this.onboardingWizard.runnerWorkspaceRoot.trim(),
          };
          await api('/config/runner/validate', { method: 'POST', body: patch });
          await api('/config/runner', { method: 'PATCH', body: patch });
          await this.loadRunnerConfig();
          this.onboardingWizard.step = 3;
          this.showToast('Runner configuration saved.', 'success');
          return;
        }

        if (this.onboardingWizard.step === 3) {
          if (!this.onboardingWizard.userId.trim()) {
            throw new Error('User ID is required.');
          }
          if (!this.onboardingWizard.userConfigPath.trim()) {
            this.onboardingWizard.userConfigPath = this.defaultUserConfigPath(this.onboardingWizard.userId);
          }

          await api('/config/users', {
            method: 'POST',
            body: {
              user_id: this.onboardingWizard.userId.trim(),
              config_path: this.onboardingWizard.userConfigPath.trim(),
            },
          });
          await this.loadUsersPage();
          this.onboardingWizard.step = 4;
          this.showToast('User registration completed.', 'success');
          return;
        }

        if (this.onboardingWizard.step === 4) {
          const providerName = this.onboardingWizard.providerName.trim();
          if (!providerName) {
            throw new Error('Provider name is required.');
          }

          const providerPatch = {
            providers: {
              registry: {
                [providerName]: {
                  provider_type: this.onboardingWizard.providerType,
                },
              },
            },
          };

          if (this.onboardingWizard.providerApiKey.trim()) {
            providerPatch.providers.registry[providerName].api_key = this.onboardingWizard.providerApiKey.trim();
          } else if (this.onboardingWizard.providerApiKeyEnv.trim()) {
            providerPatch.providers.registry[providerName].api_key_env = this.onboardingWizard.providerApiKeyEnv.trim();
          } else {
            throw new Error('Provide an API key or an API key env var name.');
          }

          await api('/config/agent/validate', { method: 'POST', body: providerPatch });
          await api('/config/agent', { method: 'PATCH', body: providerPatch });
          await this.loadAgentConfig();
          await this.refreshOnboardingStatus();
          this.onboardingWizard.providerEnvResolved = Boolean(this.onboarding.checks?.has_provider);
          this.onboardingWizard.step = 5;
          this.showToast('Provider setup saved.', 'success');
          return;
        }

        if (this.onboardingWizard.step === 5) {
          this.onboardingWizard.done = true;
          await this.refreshOnboardingStatus();
          this.showToast('Setup complete. You can now use the dashboard.', 'success');
          window.location.hash = '#/';
        }
      } catch (error) {
        this.onboardingWizard.error = error.message;
      } finally {
        this.onboardingWizard.busy = false;
      }
    },

    showToast(message, kind = 'info') {
      this.toast.message = message;
      this.toast.kind = kind;
      this.toast.visible = true;
      window.clearTimeout(this._toastTimer);
      this._toastTimer = window.setTimeout(() => {
        this.toast.visible = false;
      }, 4000);
    },
  };
}

window.app = app;
