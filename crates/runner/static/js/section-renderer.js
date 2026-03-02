/**
 * SectionRenderer — Renders schema sections as collapsible cards
 * containing fields rendered by FormRenderer.
 *
 * Sections can be:
 *  - Standard: a card with a list of fields.
 *  - Optional: same as standard but with an enable/disable toggle.
 *  - Collection: uses CollectionEditor for dynamic map/array entries.
 *
 * Exposed as window.SectionRenderer.
 */
window.SectionRenderer = (function () {
  'use strict';

  // ---------------------------------------------------------------------------
  // DOM helpers
  // ---------------------------------------------------------------------------

  function el(tag, className) {
    var node = document.createElement(tag);
    if (className) node.className = className;
    return node;
  }

  // ---------------------------------------------------------------------------
  // Main API
  // ---------------------------------------------------------------------------

  /**
   * Render a schema section as a collapsible card.
   *
   * @param {Object}   section           Section schema from /api/v1/meta/schema.
   * @param {Object}   values            Current config values (flat key→value
   *                                     or nested object for collections).
   * @param {Object}   opts
   * @param {Object}   opts.dynamicSources  Dynamic sources map.
   * @param {Array}    opts.catalog         Catalog providers (for model_picker).
   * @param {Function} opts.onChange         Called as onChange(path, newValue).
   * @param {Function} opts.onToggleSection  Called as onToggleSection(sectionId, enabled)
   *                                         for optional sections.
   * @param {boolean}  [opts.startExpanded]  Start expanded? Default: true.
   * @param {string}   [opts.idPrefix]       ID prefix for fields.
   * @returns {{ element, getValues, setValues, widgets }}
   */
  function renderSection(section, values, opts) {
    opts = opts || {};
    var startExpanded = opts.startExpanded !== false;

    var card = el('div', 'sr-section');
    card.dataset.sectionId = section.id;

    // ── Header ──────────────────────────────────────────────────
    var header = el('div', 'sr-header');
    header.setAttribute('role', 'button');
    header.setAttribute('tabindex', '0');
    header.setAttribute('aria-expanded', String(startExpanded));

    var headerLeft = el('div', 'sr-header-left');

    var chevron = el('span', 'sr-chevron');
    chevron.textContent = startExpanded ? '▾' : '▸';
    headerLeft.appendChild(chevron);

    var titleGroup = el('div', 'sr-title-group');
    var title = el('span', 'sr-title');
    title.textContent = section.label;
    titleGroup.appendChild(title);

    if (section.description) {
      var desc = el('span', 'sr-description');
      desc.textContent = section.description;
      titleGroup.appendChild(desc);
    }
    headerLeft.appendChild(titleGroup);
    header.appendChild(headerLeft);

    // Optional section toggle
    var sectionEnabled = true;
    if (section.optional_section) {
      sectionEnabled = hasAnyValues(section, values);
      var toggleWrap = el('div', 'sr-toggle-wrap');
      toggleWrap.addEventListener('click', function (e) { e.stopPropagation(); });
      var toggleLabel = el('label', 'fr-toggle fr-toggle-sm');
      var toggleInput = el('input');
      toggleInput.type = 'checkbox';
      toggleInput.checked = sectionEnabled;
      toggleInput.setAttribute('aria-label', 'Enable ' + section.label);
      var toggleSlider = el('span', 'fr-toggle-slider');
      var toggleText = el('span', 'fr-toggle-text');
      toggleText.textContent = sectionEnabled ? 'Enabled' : 'Disabled';

      toggleInput.addEventListener('change', function () {
        sectionEnabled = toggleInput.checked;
        toggleText.textContent = sectionEnabled ? 'Enabled' : 'Disabled';
        body.style.display = sectionEnabled && isExpanded ? '' : 'none';
        if (opts.onToggleSection) opts.onToggleSection(section.id, sectionEnabled);
      });

      toggleLabel.appendChild(toggleInput);
      toggleLabel.appendChild(toggleSlider);
      toggleLabel.appendChild(toggleText);
      toggleWrap.appendChild(toggleLabel);
      header.appendChild(toggleWrap);
    }

    card.appendChild(header);

    // ── Body ────────────────────────────────────────────────────
    var body = el('div', 'sr-body');
    var isExpanded = startExpanded;
    body.style.display = (isExpanded && sectionEnabled) ? '' : 'none';

    // Expand/collapse toggle
    function toggleExpand() {
      isExpanded = !isExpanded;
      body.style.display = (isExpanded && sectionEnabled) ? '' : 'none';
      chevron.textContent = isExpanded ? '▾' : '▸';
      header.setAttribute('aria-expanded', String(isExpanded));
    }

    header.addEventListener('click', toggleExpand);
    header.addEventListener('keydown', function (e) {
      if (e.key === 'Enter' || e.key === ' ') {
        e.preventDefault();
        toggleExpand();
      }
    });

    // ── Render fields ───────────────────────────────────────────
    var widgets = {};

    // If this section is a collection, delegate to CollectionEditor
    if (section.collection) {
      var collectionPlaceholder = el('div', 'sr-collection-placeholder');
      collectionPlaceholder.dataset.sectionId = section.id;
      body.appendChild(collectionPlaceholder);
      card.appendChild(body);
      return {
        element: card,
        getValues: function () { return {}; },
        setValues: function () {},
        widgets: widgets,
        collectionPlaceholder: collectionPlaceholder,
        collectionMeta: section.collection,
        sectionSchema: section,
        isEnabled: function () { return sectionEnabled; },
      };
    }

    // Standard fields
    section.fields.forEach(function (fieldSchema) {
      var fieldValue = resolveFieldValue(fieldSchema.path, values);
      var widget = window.FormRenderer.renderField(fieldSchema, fieldValue, {
        dynamicSources: opts.dynamicSources,
        catalog: opts.catalog,
        onChange: function (path, newValue) {
          if (opts.onChange) opts.onChange(path, newValue);
        },
        idPrefix: opts.idPrefix || section.id,
      });
      widgets[fieldSchema.path] = widget;
      body.appendChild(widget.element);
    });

    // Sub-sections
    if (section.subsections && section.subsections.length) {
      section.subsections.forEach(function (sub) {
        var subResult = renderSection(sub, values, {
          dynamicSources: opts.dynamicSources,
          catalog: opts.catalog,
          onChange: opts.onChange,
          startExpanded: false,
          idPrefix: opts.idPrefix || sub.id,
        });
        body.appendChild(subResult.element);
        // Merge sub-section widgets
        Object.keys(subResult.widgets).forEach(function (k) {
          widgets[k] = subResult.widgets[k];
        });
      });
    }

    card.appendChild(body);

    return {
      element: card,
      widgets: widgets,
      isEnabled: function () { return sectionEnabled; },
      getValues: function () {
        var result = {};
        Object.keys(widgets).forEach(function (path) {
          result[path] = widgets[path].getValue();
        });
        return result;
      },
      setValues: function (newValues) {
        Object.keys(widgets).forEach(function (path) {
          var v = resolveFieldValue(path, newValues);
          widgets[path].setValue(v);
        });
      },
    };
  }

  // ---------------------------------------------------------------------------
  // Helpers
  // ---------------------------------------------------------------------------

  /**
   * Resolve a dotted path from a nested object.
   * e.g., resolveFieldValue('runtime.max_cost', { runtime: { max_cost: 5 } }) → 5
   */
  function resolveFieldValue(path, obj) {
    if (!obj || typeof obj !== 'object') return undefined;
    var parts = path.split('.');
    var current = obj;
    for (var i = 0; i < parts.length; i++) {
      if (current == null || typeof current !== 'object') return undefined;
      current = current[parts[i]];
    }
    return current;
  }

  /**
   * Check if an optional section has any existing values in the config.
   */
  function hasAnyValues(section, values) {
    if (!values || typeof values !== 'object') return false;
    return section.fields.some(function (f) {
      var v = resolveFieldValue(f.path, values);
      return v !== undefined && v !== null;
    });
  }

  // ---------------------------------------------------------------------------
  // Public API
  // ---------------------------------------------------------------------------

  return {
    renderSection: renderSection,
    resolveFieldValue: resolveFieldValue,
  };
})();
