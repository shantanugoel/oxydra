/**
 * FormRenderer — Renders config fields with appropriate input widgets
 * based on schema metadata from /api/v1/meta/schema.
 *
 * Each field is self-contained: label, description, input widget,
 * nullable controls, and error display. The consumer provides an
 * onChange callback and can read/write values programmatically.
 *
 * Exposed as window.FormRenderer.
 */
window.FormRenderer = (function () {
  'use strict';

  const SECRET_SENTINEL = '__UNCHANGED__';

  // ---------------------------------------------------------------------------
  // DOM helpers
  // ---------------------------------------------------------------------------

  function el(tag, className) {
    const node = document.createElement(tag);
    if (className) node.className = className;
    return node;
  }

  function uid(prefix) {
    return prefix + '-' + Math.random().toString(36).slice(2, 9);
  }

  function formatContextWindow(tokens) {
    if (!tokens) return '';
    if (tokens >= 1000000) return (tokens / 1000000).toFixed(1) + 'M';
    if (tokens >= 1000) return Math.round(tokens / 1000) + 'K';
    return String(tokens);
  }

  // ---------------------------------------------------------------------------
  // Main API
  // ---------------------------------------------------------------------------

  /**
   * Render a single field.
   *
   * @param {Object}   schema  Field metadata from the schema endpoint.
   * @param {*}        value   Current config value (null/undefined = unset).
   * @param {Object}   opts
   * @param {Object}   opts.dynamicSources  Dynamic sources map.
   * @param {Array}    opts.catalog         Catalog providers array.
   * @param {Function} opts.onChange         Called as onChange(path, newValue).
   * @param {string}   [opts.idPrefix]      ID prefix to avoid collisions.
   * @returns {{ element, getValue, setValue, setError, clearError }}
   */
  function renderField(schema, value, opts) {
    opts = opts || {};
    var container = el('div', 'fr-field');
    container.dataset.fieldPath = schema.path;

    var fieldId = uid(opts.idPrefix || 'f');

    // ── Label ───────────────────────────────────────────────────
    var label = el('label', 'fr-label');
    label.setAttribute('for', fieldId);
    label.textContent = schema.label;
    if (schema.required) {
      var star = el('span', 'fr-required');
      star.textContent = ' *';
      label.appendChild(star);
    }
    container.appendChild(label);

    // ── Description ─────────────────────────────────────────────
    var descId = null;
    if (schema.description) {
      var desc = el('p', 'fr-description');
      descId = fieldId + '-desc';
      desc.id = descId;
      desc.textContent = schema.description;
      container.appendChild(desc);
    }

    // ── Input widget ────────────────────────────────────────────
    var widget = renderInput(schema, value, fieldId, descId, opts);
    container.appendChild(widget.element);

    // ── Error display ───────────────────────────────────────────
    var errorEl = el('p', 'fr-error');
    errorEl.style.display = 'none';
    errorEl.setAttribute('role', 'alert');
    container.appendChild(errorEl);

    return {
      element: container,
      getValue: widget.getValue,
      setValue: widget.setValue,
      setError: function (msg) {
        errorEl.textContent = msg || '';
        errorEl.style.display = msg ? '' : 'none';
        container.classList.toggle('fr-has-error', Boolean(msg));
      },
      clearError: function () {
        errorEl.textContent = '';
        errorEl.style.display = 'none';
        container.classList.remove('fr-has-error');
      },
    };
  }

  // ---------------------------------------------------------------------------
  // Input dispatcher
  // ---------------------------------------------------------------------------

  function renderInput(schema, value, fieldId, descId, opts) {
    var renderers = {
      text: renderText,
      number: renderNumber,
      boolean: renderBoolean,
      secret: renderSecret,
      select: renderSelect,
      select_dynamic: renderSelectDynamic,
      model_picker: renderModelPicker,
      multiline: renderMultiline,
      multi_select: renderMultiSelect,
      tag_list: renderTagList,
      key_value_map: renderKeyValueMap,
      readonly: renderReadonly,
    };
    var fn = renderers[schema.input_type] || renderText;
    return fn(schema, value, fieldId, descId, opts);
  }

  // ---------------------------------------------------------------------------
  // aria helper
  // ---------------------------------------------------------------------------

  function setAria(input, descId) {
    if (descId) input.setAttribute('aria-describedby', descId);
  }

  // ---------------------------------------------------------------------------
  // text
  // ---------------------------------------------------------------------------

  function renderText(schema, value, fieldId, descId, opts) {
    var wrapper = el('div', 'fr-input-wrap');
    var input = el('input', 'text-input');
    input.type = 'text';
    input.id = fieldId;
    input.value = value != null ? String(value) : '';
    if (schema.nullable && value == null) {
      input.placeholder = 'Not set';
    }
    setAria(input, descId);

    // Datalist suggestions from dynamic_source
    if (schema.dynamic_source && opts.dynamicSources) {
      var items = opts.dynamicSources[schema.dynamic_source];
      if (items && items.length) {
        var dlId = fieldId + '-dl';
        var dl = el('datalist');
        dl.id = dlId;
        items.forEach(function (item) {
          var opt = el('option');
          opt.value = typeof item === 'string' ? item : item.value;
          dl.appendChild(opt);
        });
        wrapper.appendChild(dl);
        input.setAttribute('list', dlId);
      }
    }

    input.addEventListener('input', function () {
      var v = input.value;
      if (schema.nullable && v === '') v = null;
      if (opts.onChange) opts.onChange(schema.path, v);
    });

    wrapper.appendChild(input);
    return {
      element: wrapper,
      getValue: function () {
        var v = input.value;
        if (schema.nullable && v === '') return null;
        return v;
      },
      setValue: function (v) {
        input.value = v != null ? String(v) : '';
      },
    };
  }

  // ---------------------------------------------------------------------------
  // number
  // ---------------------------------------------------------------------------

  function renderNumber(schema, value, fieldId, descId, opts) {
    var wrapper = el('div', 'fr-input-wrap');
    var input = el('input', 'text-input');
    input.type = 'number';
    input.id = fieldId;
    setAria(input, descId);

    var c = schema.constraints || {};
    if (c.min != null) input.min = c.min;
    if (c.max != null) input.max = c.max;
    if (c.step != null) input.step = c.step;

    if (value != null) {
      input.value = value;
    } else {
      input.value = '';
      if (schema.nullable) input.placeholder = 'Not set';
    }

    input.addEventListener('input', function () {
      var raw = input.value;
      if (raw === '') {
        if (opts.onChange) opts.onChange(schema.path, schema.nullable ? null : undefined);
        return;
      }
      var n = Number(raw);
      if (Number.isFinite(n)) {
        if (opts.onChange) opts.onChange(schema.path, n);
      }
    });

    wrapper.appendChild(input);
    return {
      element: wrapper,
      getValue: function () {
        var raw = input.value;
        if (raw === '') return schema.nullable ? null : undefined;
        var n = Number(raw);
        return Number.isFinite(n) ? n : undefined;
      },
      setValue: function (v) {
        input.value = v != null ? v : '';
      },
    };
  }

  // ---------------------------------------------------------------------------
  // boolean
  // ---------------------------------------------------------------------------

  function renderBoolean(schema, value, fieldId, descId, opts) {
    // Nullable booleans use a three-state select
    if (schema.nullable) {
      return renderTriStateBoolean(schema, value, fieldId, descId, opts);
    }

    var wrapper = el('label', 'fr-toggle');
    wrapper.setAttribute('for', fieldId);

    var input = el('input');
    input.type = 'checkbox';
    input.id = fieldId;
    input.checked = Boolean(value);
    setAria(input, descId);

    var slider = el('span', 'fr-toggle-slider');
    var text = el('span', 'fr-toggle-text');
    text.textContent = input.checked ? 'Enabled' : 'Disabled';

    input.addEventListener('change', function () {
      text.textContent = input.checked ? 'Enabled' : 'Disabled';
      if (opts.onChange) opts.onChange(schema.path, input.checked);
    });

    wrapper.appendChild(input);
    wrapper.appendChild(slider);
    wrapper.appendChild(text);

    return {
      element: wrapper,
      getValue: function () { return input.checked; },
      setValue: function (v) {
        input.checked = Boolean(v);
        text.textContent = input.checked ? 'Enabled' : 'Disabled';
      },
    };
  }

  function renderTriStateBoolean(schema, value, fieldId, descId, opts) {
    var wrapper = el('div', 'fr-input-wrap');
    var sel = el('select', 'text-input fr-tristate');
    sel.id = fieldId;
    setAria(sel, descId);

    var optUnset = el('option');
    optUnset.value = '__null__';
    optUnset.textContent = '— Not set (use default) —';
    sel.appendChild(optUnset);

    var optTrue = el('option');
    optTrue.value = 'true';
    optTrue.textContent = 'Yes';
    sel.appendChild(optTrue);

    var optFalse = el('option');
    optFalse.value = 'false';
    optFalse.textContent = 'No';
    sel.appendChild(optFalse);

    if (value === true) sel.value = 'true';
    else if (value === false) sel.value = 'false';
    else sel.value = '__null__';

    sel.addEventListener('change', function () {
      var v = sel.value;
      if (v === '__null__') v = null;
      else v = v === 'true';
      if (opts.onChange) opts.onChange(schema.path, v);
    });

    wrapper.appendChild(sel);
    return {
      element: wrapper,
      getValue: function () {
        if (sel.value === '__null__') return null;
        return sel.value === 'true';
      },
      setValue: function (v) {
        if (v === true) sel.value = 'true';
        else if (v === false) sel.value = 'false';
        else sel.value = '__null__';
      },
    };
  }

  // ---------------------------------------------------------------------------
  // secret
  // ---------------------------------------------------------------------------

  function renderSecret(schema, value, fieldId, descId, opts) {
    var wrapper = el('div', 'fr-secret-row');
    var input = el('input', 'text-input');
    input.type = 'password';
    input.id = fieldId;
    input.placeholder = '••••••••';
    input.autocomplete = 'off';
    setAria(input, descId);

    var touched = false;

    input.addEventListener('input', function () {
      touched = true;
      if (opts.onChange) opts.onChange(schema.path, input.value);
    });

    var clearBtn = el('button', 'btn btn-muted btn-sm');
    clearBtn.type = 'button';
    clearBtn.textContent = 'Clear';
    clearBtn.addEventListener('click', function () {
      input.value = '';
      touched = true;
      if (opts.onChange) opts.onChange(schema.path, '');
    });

    wrapper.appendChild(input);
    wrapper.appendChild(clearBtn);

    return {
      element: wrapper,
      getValue: function () {
        if (!touched) return SECRET_SENTINEL;
        return input.value;
      },
      setValue: function () {
        // Secrets are write-only; cannot be read back from the API.
        input.value = '';
        touched = false;
      },
    };
  }

  // ---------------------------------------------------------------------------
  // select
  // ---------------------------------------------------------------------------

  function renderSelect(schema, value, fieldId, descId, opts) {
    var wrapper = el('div', 'fr-input-wrap');
    var sel = el('select', 'text-input');
    sel.id = fieldId;
    setAria(sel, descId);

    if (schema.nullable) {
      var unsetOpt = el('option');
      unsetOpt.value = '__null__';
      unsetOpt.textContent = '— Not set —';
      sel.appendChild(unsetOpt);
    }

    var options = schema.enum_options || [];
    options.forEach(function (o) {
      var opt = el('option');
      opt.value = o.value;
      opt.textContent = o.label;
      sel.appendChild(opt);
    });

    if (value != null) {
      sel.value = String(value);
      // If the value isn't in the list, add it as a custom option
      if (sel.value !== String(value)) {
        var custom = el('option');
        custom.value = String(value);
        custom.textContent = String(value) + ' (custom)';
        sel.insertBefore(custom, sel.firstChild.nextSibling || null);
        sel.value = String(value);
      }
    } else if (schema.nullable) {
      sel.value = '__null__';
    }

    sel.addEventListener('change', function () {
      var v = sel.value;
      if (v === '__null__') v = null;
      if (opts.onChange) opts.onChange(schema.path, v);
    });

    wrapper.appendChild(sel);
    return {
      element: wrapper,
      getValue: function () {
        var v = sel.value;
        return v === '__null__' ? null : v;
      },
      setValue: function (v) {
        if (v == null && schema.nullable) sel.value = '__null__';
        else sel.value = String(v);
      },
    };
  }

  // ---------------------------------------------------------------------------
  // select_dynamic
  // ---------------------------------------------------------------------------

  function renderSelectDynamic(schema, value, fieldId, descId, opts) {
    var wrapper = el('div', 'fr-input-wrap fr-select-dynamic');
    var items = [];
    if (schema.dynamic_source && opts.dynamicSources) {
      var src = opts.dynamicSources[schema.dynamic_source];
      if (src) {
        items = src.map(function (s) {
          if (typeof s === 'string') return { value: s, label: s };
          return s;
        });
      }
    }

    var customMode = false;
    var selContainer = el('div', 'fr-select-dynamic-sel');
    var customContainer = el('div', 'fr-select-dynamic-custom');
    customContainer.style.display = 'none';

    // Select element
    var sel = el('select', 'text-input');
    sel.id = fieldId;
    setAria(sel, descId);

    if (schema.nullable) {
      var unsetOpt = el('option');
      unsetOpt.value = '__null__';
      unsetOpt.textContent = '— Not set —';
      sel.appendChild(unsetOpt);
    }

    items.forEach(function (item) {
      var opt = el('option');
      opt.value = item.value;
      opt.textContent = item.label;
      sel.appendChild(opt);
    });

    if (schema.allow_custom) {
      var customOpt = el('option');
      customOpt.value = '__custom__';
      customOpt.textContent = '— Custom value… —';
      sel.appendChild(customOpt);
    }

    // Custom text input
    var customInput = el('input', 'text-input');
    customInput.type = 'text';
    customInput.placeholder = 'Enter custom value';

    var backBtn = el('button', 'btn btn-muted btn-sm');
    backBtn.type = 'button';
    backBtn.textContent = '← Back to list';
    backBtn.addEventListener('click', function () {
      customMode = false;
      customContainer.style.display = 'none';
      selContainer.style.display = '';
      sel.value = value != null ? String(value) : (schema.nullable ? '__null__' : '');
      if (opts.onChange) opts.onChange(schema.path, getVal());
    });

    customContainer.appendChild(customInput);
    customContainer.appendChild(backBtn);

    customInput.addEventListener('input', function () {
      if (opts.onChange) opts.onChange(schema.path, customInput.value || null);
    });

    // Set initial value
    var knownValues = items.map(function (i) { return i.value; });
    if (value != null && knownValues.indexOf(String(value)) === -1 && schema.allow_custom) {
      // Value is custom
      customMode = true;
      selContainer.style.display = 'none';
      customContainer.style.display = '';
      customInput.value = String(value);
    } else {
      if (value != null) sel.value = String(value);
      else if (schema.nullable) sel.value = '__null__';
    }

    sel.addEventListener('change', function () {
      if (sel.value === '__custom__') {
        customMode = true;
        selContainer.style.display = 'none';
        customContainer.style.display = '';
        customInput.value = '';
        customInput.focus();
        return;
      }
      var v = sel.value;
      if (v === '__null__') v = null;
      if (opts.onChange) opts.onChange(schema.path, v);
    });

    selContainer.appendChild(sel);
    wrapper.appendChild(selContainer);
    wrapper.appendChild(customContainer);

    function getVal() {
      if (customMode) {
        var cv = customInput.value;
        return cv || (schema.nullable ? null : '');
      }
      var sv = sel.value;
      if (sv === '__null__' || sv === '__custom__') return null;
      return sv;
    }

    return {
      element: wrapper,
      getValue: getVal,
      setValue: function (v) {
        if (v != null && knownValues.indexOf(String(v)) === -1 && schema.allow_custom) {
          customMode = true;
          selContainer.style.display = 'none';
          customContainer.style.display = '';
          customInput.value = String(v);
        } else {
          customMode = false;
          customContainer.style.display = 'none';
          selContainer.style.display = '';
          if (v == null && schema.nullable) sel.value = '__null__';
          else if (v != null) sel.value = String(v);
        }
      },
    };
  }

  // ---------------------------------------------------------------------------
  // model_picker
  // ---------------------------------------------------------------------------

  function renderModelPicker(schema, value, fieldId, descId, opts) {
    var wrapper = el('div', 'fr-model-picker');
    var catalog = opts.catalog || [];

    // Build flat list with provider grouping
    var allModels = [];
    catalog.forEach(function (provider) {
      (provider.models || []).forEach(function (m) {
        allModels.push({
          id: m.id,
          name: m.name,
          provider: provider.id,
          providerName: provider.name,
          reasoning: m.reasoning,
          toolCall: m.tool_call,
          attachment: m.attachment,
          contextWindow: m.context_window,
        });
      });
    });

    var isCustomMode = false;
    var currentValue = value != null ? String(value) : '';
    var isOpen = false;

    // ── Trigger button ──────────────────────────────────────────
    var trigger = el('button', 'fr-mp-trigger text-input');
    trigger.type = 'button';
    trigger.id = fieldId;
    setAria(trigger, descId);
    updateTriggerText();

    // ── Dropdown panel ──────────────────────────────────────────
    var dropdown = el('div', 'fr-mp-dropdown');
    dropdown.style.display = 'none';

    // Search input
    var searchInput = el('input', 'fr-mp-search text-input');
    searchInput.type = 'text';
    searchInput.placeholder = 'Search models…';
    dropdown.appendChild(searchInput);

    // Model list container
    var listContainer = el('div', 'fr-mp-list');
    dropdown.appendChild(listContainer);

    // Custom mode
    if (schema.allow_custom) {
      var customRow = el('div', 'fr-mp-custom-row');
      var customBtn = el('button', 'fr-mp-custom-btn');
      customBtn.type = 'button';
      customBtn.textContent = '✏ Use custom model ID…';
      customBtn.addEventListener('click', function () {
        isCustomMode = true;
        closeDropdown();
        showCustomInput();
      });
      customRow.appendChild(customBtn);
      dropdown.appendChild(customRow);
    }

    // ── Custom input (hidden by default) ────────────────────────
    var customWrap = el('div', 'fr-mp-custom-wrap');
    customWrap.style.display = 'none';
    var customInput = el('input', 'text-input');
    customInput.type = 'text';
    customInput.placeholder = 'Enter model ID (e.g., gpt-4o)';
    var backLink = el('button', 'btn btn-muted btn-sm');
    backLink.type = 'button';
    backLink.textContent = '← Back to catalog';
    backLink.addEventListener('click', function () {
      isCustomMode = false;
      customWrap.style.display = 'none';
      trigger.style.display = '';
      updateTriggerText();
    });
    customInput.addEventListener('input', function () {
      currentValue = customInput.value;
      if (opts.onChange) opts.onChange(schema.path, currentValue || (schema.nullable ? null : ''));
    });
    customWrap.appendChild(customInput);
    customWrap.appendChild(backLink);

    wrapper.appendChild(trigger);
    wrapper.appendChild(dropdown);
    wrapper.appendChild(customWrap);

    // Check if current value is custom (not in catalog)
    if (currentValue && !allModels.some(function (m) { return m.id === currentValue; })) {
      if (schema.allow_custom && currentValue) {
        isCustomMode = true;
        showCustomInput();
      }
    }

    // ── Event handlers ──────────────────────────────────────────

    trigger.addEventListener('click', function () {
      if (isOpen) closeDropdown();
      else openDropdown();
    });

    searchInput.addEventListener('input', function () {
      renderModelList(searchInput.value.toLowerCase());
    });

    // Close on outside click
    function onDocClick(e) {
      if (!wrapper.contains(e.target)) closeDropdown();
    }

    // Keyboard navigation
    searchInput.addEventListener('keydown', function (e) {
      if (e.key === 'Escape') {
        closeDropdown();
        trigger.focus();
      }
    });

    function openDropdown() {
      isOpen = true;
      dropdown.style.display = '';
      searchInput.value = '';
      renderModelList('');
      searchInput.focus();
      document.addEventListener('click', onDocClick, true);
    }

    function closeDropdown() {
      isOpen = false;
      dropdown.style.display = 'none';
      document.removeEventListener('click', onDocClick, true);
    }

    function showCustomInput() {
      trigger.style.display = 'none';
      customWrap.style.display = '';
      customInput.value = currentValue;
      customInput.focus();
    }

    function selectModel(modelId) {
      currentValue = modelId;
      isCustomMode = false;
      customWrap.style.display = 'none';
      trigger.style.display = '';
      updateTriggerText();
      closeDropdown();
      if (opts.onChange) opts.onChange(schema.path, currentValue);
    }

    function updateTriggerText() {
      var model = allModels.find(function (m) { return m.id === currentValue; });
      if (model) {
        trigger.textContent = '';
        var nameSpan = el('span', 'fr-mp-selected-name');
        nameSpan.textContent = model.name || model.id;
        trigger.appendChild(nameSpan);
        if (model.contextWindow) {
          var ctx = el('span', 'fr-mp-selected-ctx');
          ctx.textContent = formatContextWindow(model.contextWindow) + ' ctx';
          trigger.appendChild(ctx);
        }
        var arrow = el('span', 'fr-mp-arrow');
        arrow.textContent = '▾';
        trigger.appendChild(arrow);
      } else if (currentValue) {
        trigger.textContent = currentValue;
        var arrowC = el('span', 'fr-mp-arrow');
        arrowC.textContent = ' ▾';
        trigger.appendChild(arrowC);
      } else {
        trigger.textContent = schema.nullable ? '— Not set —' : 'Select a model…';
        var arrowD = el('span', 'fr-mp-arrow');
        arrowD.textContent = ' ▾';
        trigger.appendChild(arrowD);
      }
    }

    function renderModelList(query) {
      listContainer.innerHTML = '';
      var grouped = {};
      allModels.forEach(function (m) {
        if (query) {
          var text = (m.id + ' ' + m.name + ' ' + m.providerName).toLowerCase();
          if (text.indexOf(query) === -1) return;
        }
        if (!grouped[m.provider]) grouped[m.provider] = { name: m.providerName, models: [] };
        grouped[m.provider].models.push(m);
      });

      var providerIds = Object.keys(grouped);
      if (providerIds.length === 0) {
        var empty = el('div', 'fr-mp-empty');
        empty.textContent = query ? 'No models match your search.' : 'No models available.';
        listContainer.appendChild(empty);
        return;
      }

      providerIds.forEach(function (pid) {
        var group = grouped[pid];
        var header = el('div', 'fr-mp-group-header');
        header.textContent = group.name || pid;
        listContainer.appendChild(header);

        group.models.forEach(function (m) {
          var item = el('button', 'fr-mp-item');
          item.type = 'button';
          if (m.id === currentValue) item.classList.add('fr-mp-item-selected');

          var nameRow = el('span', 'fr-mp-item-name');
          nameRow.textContent = m.name || m.id;
          item.appendChild(nameRow);

          var meta = el('span', 'fr-mp-item-meta');
          var badges = [];
          if (m.contextWindow) badges.push(formatContextWindow(m.contextWindow) + ' ctx');
          if (m.reasoning) badges.push('reasoning');
          if (m.toolCall) badges.push('tools');
          if (m.attachment) badges.push('attachments');
          meta.textContent = badges.join(' · ');
          item.appendChild(meta);

          item.addEventListener('click', function (e) {
            e.stopPropagation();
            selectModel(m.id);
          });

          listContainer.appendChild(item);
        });
      });
    }

    return {
      element: wrapper,
      getValue: function () {
        if (isCustomMode) {
          var cv = customInput.value;
          return cv || (schema.nullable ? null : '');
        }
        return currentValue || (schema.nullable ? null : '');
      },
      setValue: function (v) {
        currentValue = v != null ? String(v) : '';
        isCustomMode = false;
        customWrap.style.display = 'none';
        trigger.style.display = '';
        if (currentValue && !allModels.some(function (m) { return m.id === currentValue; }) && schema.allow_custom) {
          isCustomMode = true;
          showCustomInput();
        } else {
          updateTriggerText();
        }
      },
    };
  }

  // ---------------------------------------------------------------------------
  // multiline
  // ---------------------------------------------------------------------------

  function renderMultiline(schema, value, fieldId, descId, opts) {
    var wrapper = el('div', 'fr-input-wrap');
    var textarea = el('textarea', 'text-input fr-multiline');
    textarea.id = fieldId;
    textarea.rows = 6;
    textarea.value = value != null ? String(value) : '';
    if (schema.nullable && value == null) textarea.placeholder = 'Not set';
    setAria(textarea, descId);

    textarea.addEventListener('input', function () {
      var v = textarea.value;
      if (schema.nullable && v === '') v = null;
      if (opts.onChange) opts.onChange(schema.path, v);
    });

    wrapper.appendChild(textarea);
    return {
      element: wrapper,
      getValue: function () {
        var v = textarea.value;
        return (schema.nullable && v === '') ? null : v;
      },
      setValue: function (v) {
        textarea.value = v != null ? String(v) : '';
      },
    };
  }

  // ---------------------------------------------------------------------------
  // multi_select
  // ---------------------------------------------------------------------------

  function renderMultiSelect(schema, value, fieldId, descId, opts) {
    var wrapper = el('div', 'fr-multi-select');
    wrapper.id = fieldId;
    setAria(wrapper, descId);

    // Resolve options
    var options = [];
    if (schema.dynamic_source && opts.dynamicSources && opts.dynamicSources[schema.dynamic_source]) {
      var src = opts.dynamicSources[schema.dynamic_source];
      options = src.map(function (s) {
        return typeof s === 'string' ? { value: s, label: s } : s;
      });
    } else if (schema.enum_options) {
      options = schema.enum_options;
    }

    var selected = new Set(Array.isArray(value) ? value : []);

    // Toggle all / none
    var controls = el('div', 'fr-ms-controls');
    var selectAll = el('button', 'btn btn-muted btn-sm');
    selectAll.type = 'button';
    selectAll.textContent = 'Select all';
    selectAll.addEventListener('click', function () {
      options.forEach(function (o) { selected.add(o.value); });
      updateCheckboxes();
      fireChange();
    });
    var selectNone = el('button', 'btn btn-muted btn-sm');
    selectNone.type = 'button';
    selectNone.textContent = 'None';
    selectNone.addEventListener('click', function () {
      selected.clear();
      updateCheckboxes();
      fireChange();
    });
    controls.appendChild(selectAll);
    controls.appendChild(selectNone);
    wrapper.appendChild(controls);

    // Checkbox list
    var listEl = el('div', 'fr-ms-list');
    var checkboxes = [];

    options.forEach(function (o) {
      var row = el('label', 'fr-ms-item');
      var cb = el('input');
      cb.type = 'checkbox';
      cb.value = o.value;
      cb.checked = selected.has(o.value);
      cb.addEventListener('change', function () {
        if (cb.checked) selected.add(o.value);
        else selected.delete(o.value);
        fireChange();
      });
      var text = el('span');
      text.textContent = o.label;
      row.appendChild(cb);
      row.appendChild(text);
      listEl.appendChild(row);
      checkboxes.push({ checkbox: cb, value: o.value });
    });

    wrapper.appendChild(listEl);

    function updateCheckboxes() {
      checkboxes.forEach(function (item) {
        item.checkbox.checked = selected.has(item.value);
      });
    }

    function fireChange() {
      var arr = Array.from(selected);
      if (opts.onChange) opts.onChange(schema.path, arr.length ? arr : (schema.nullable ? null : []));
    }

    return {
      element: wrapper,
      getValue: function () {
        var arr = Array.from(selected);
        return arr.length ? arr : (schema.nullable ? null : []);
      },
      setValue: function (v) {
        selected.clear();
        if (Array.isArray(v)) v.forEach(function (x) { selected.add(x); });
        updateCheckboxes();
      },
    };
  }

  // ---------------------------------------------------------------------------
  // tag_list
  // ---------------------------------------------------------------------------

  function renderTagList(schema, value, fieldId, descId, opts) {
    var wrapper = el('div', 'fr-tag-list');
    wrapper.id = fieldId;
    setAria(wrapper, descId);

    var tags = Array.isArray(value) ? value.slice() : [];

    var chipsContainer = el('div', 'fr-tag-chips');
    var inputRow = el('div', 'fr-tag-input-row');
    var input = el('input', 'text-input fr-tag-input');
    input.type = 'text';
    input.placeholder = 'Type and press Enter to add…';

    inputRow.appendChild(input);
    wrapper.appendChild(chipsContainer);
    wrapper.appendChild(inputRow);

    function renderChips() {
      chipsContainer.innerHTML = '';
      tags.forEach(function (tag, idx) {
        var chip = el('span', 'fr-tag-chip');
        var text = el('span', 'fr-tag-chip-text');
        text.textContent = tag;
        var removeBtn = el('button', 'fr-tag-chip-remove');
        removeBtn.type = 'button';
        removeBtn.textContent = '×';
        removeBtn.setAttribute('aria-label', 'Remove ' + tag);
        removeBtn.addEventListener('click', function () {
          tags.splice(idx, 1);
          renderChips();
          fireChange();
        });
        chip.appendChild(text);
        chip.appendChild(removeBtn);
        chipsContainer.appendChild(chip);
      });
    }

    function addTag(raw) {
      var t = raw.trim();
      if (t && tags.indexOf(t) === -1) {
        tags.push(t);
        renderChips();
        fireChange();
      }
    }

    function fireChange() {
      var val = tags.length ? tags.slice() : (schema.nullable ? null : []);
      if (opts.onChange) opts.onChange(schema.path, val);
    }

    input.addEventListener('keydown', function (e) {
      if (e.key === 'Enter' || e.key === ',') {
        e.preventDefault();
        addTag(input.value);
        input.value = '';
      }
      if (e.key === 'Backspace' && input.value === '' && tags.length) {
        tags.pop();
        renderChips();
        fireChange();
      }
    });

    input.addEventListener('blur', function () {
      if (input.value.trim()) {
        addTag(input.value);
        input.value = '';
      }
    });

    renderChips();

    return {
      element: wrapper,
      getValue: function () {
        return tags.length ? tags.slice() : (schema.nullable ? null : []);
      },
      setValue: function (v) {
        tags = Array.isArray(v) ? v.slice() : [];
        renderChips();
      },
    };
  }

  // ---------------------------------------------------------------------------
  // key_value_map
  // ---------------------------------------------------------------------------

  function renderKeyValueMap(schema, value, fieldId, descId, opts) {
    var wrapper = el('div', 'fr-kv-map');
    wrapper.id = fieldId;
    setAria(wrapper, descId);

    // Internal state: array of { key, value } pairs
    var entries = [];
    if (value && typeof value === 'object' && !Array.isArray(value)) {
      Object.keys(value).forEach(function (k) {
        entries.push({ key: k, value: String(value[k]) });
      });
    }

    var rowsContainer = el('div', 'fr-kv-rows');
    wrapper.appendChild(rowsContainer);

    var addBtn = el('button', 'btn btn-muted btn-sm');
    addBtn.type = 'button';
    addBtn.textContent = '+ Add entry';
    addBtn.addEventListener('click', function () {
      entries.push({ key: '', value: '' });
      renderRows();
    });
    wrapper.appendChild(addBtn);

    function renderRows() {
      rowsContainer.innerHTML = '';
      entries.forEach(function (entry, idx) {
        var row = el('div', 'fr-kv-row');

        var keyInput = el('input', 'text-input fr-kv-key');
        keyInput.type = 'text';
        keyInput.placeholder = 'Key';
        keyInput.value = entry.key;
        keyInput.addEventListener('input', function () {
          entry.key = keyInput.value;
          fireChange();
        });

        var valInput = el('input', 'text-input fr-kv-value');
        valInput.type = 'text';
        valInput.placeholder = 'Value';
        valInput.value = entry.value;
        valInput.addEventListener('input', function () {
          entry.value = valInput.value;
          fireChange();
        });

        var removeBtn = el('button', 'btn btn-danger btn-sm fr-kv-remove');
        removeBtn.type = 'button';
        removeBtn.textContent = '−';
        removeBtn.setAttribute('aria-label', 'Remove entry');
        removeBtn.addEventListener('click', function () {
          entries.splice(idx, 1);
          renderRows();
          fireChange();
        });

        row.appendChild(keyInput);
        row.appendChild(valInput);
        row.appendChild(removeBtn);
        rowsContainer.appendChild(row);
      });
    }

    function fireChange() {
      var result = {};
      entries.forEach(function (e) {
        if (e.key) result[e.key] = e.value;
      });
      var hasEntries = Object.keys(result).length > 0;
      if (opts.onChange) opts.onChange(schema.path, hasEntries ? result : (schema.nullable ? null : {}));
    }

    renderRows();

    return {
      element: wrapper,
      getValue: function () {
        var result = {};
        entries.forEach(function (e) {
          if (e.key) result[e.key] = e.value;
        });
        var hasEntries = Object.keys(result).length > 0;
        return hasEntries ? result : (schema.nullable ? null : {});
      },
      setValue: function (v) {
        entries = [];
        if (v && typeof v === 'object' && !Array.isArray(v)) {
          Object.keys(v).forEach(function (k) {
            entries.push({ key: k, value: String(v[k]) });
          });
        }
        renderRows();
      },
    };
  }

  // ---------------------------------------------------------------------------
  // readonly
  // ---------------------------------------------------------------------------

  function renderReadonly(schema, value, fieldId, descId) {
    var wrapper = el('div', 'fr-input-wrap');
    var input = el('input', 'text-input fr-readonly');
    input.type = 'text';
    input.id = fieldId;
    input.readOnly = true;
    input.disabled = true;
    input.value = value != null ? String(value) : (schema.default != null ? String(schema.default) : '');
    setAria(input, descId);

    wrapper.appendChild(input);
    return {
      element: wrapper,
      getValue: function () { return input.value; },
      setValue: function (v) { input.value = v != null ? String(v) : ''; },
    };
  }

  // ---------------------------------------------------------------------------
  // Public API
  // ---------------------------------------------------------------------------

  return {
    renderField: renderField,
    SECRET_SENTINEL: SECRET_SENTINEL,
  };
})();
