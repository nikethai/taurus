// Minimal stand-in for the design canvas runtime, enough to render a board
// outside the canvas: {{path}} values, <sc-for>, <sc-if>, <helmet>, event
// handlers and setState. Props come from the query string (?dark=1&accent=Jade)
// with the board's data-props defaults.
(function () {
  window.DCLogic = class {
    constructor(props) { this.props = props; this.state = this.state || {}; }
    setState(patch) { Object.assign(this.state, patch); window.__dcRender(); }
  };
  function lookup(expr, scope) {
    expr = expr.trim();
    if (expr === 'true') return true;
    if (expr === 'false') return false;
    if (/^-?\d+(\.\d+)?$/.test(expr)) return Number(expr);
    var parts = expr.split('.'), v = scope;
    for (var i = 0; i < parts.length; i++) { if (v == null) return undefined; v = v[parts[i]]; }
    return v;
  }
  var ONE = /^\{\{([^}]*)\}\}$/;
  function interp(s, scope) {
    return s.replace(/\{\{([^}]*)\}\}/g, function (_, e) { var v = lookup(e, scope); return v == null ? '' : String(v); });
  }
  function renderInto(parent, nodes, scope) {
    nodes.forEach(function (n) {
      if (n.nodeType === 3) { parent.appendChild(document.createTextNode(interp(n.nodeValue, scope))); return; }
      if (n.nodeType !== 1) return;
      var tag = n.localName;
      if (tag === 'sc-for') {
        var list = lookup(n.getAttribute('list').match(ONE)[1], scope) || [];
        var as = n.getAttribute('as');
        list.forEach(function (item) { var s = Object.create(scope); s[as] = item; renderInto(parent, Array.from(n.childNodes), s); });
        return;
      }
      if (tag === 'sc-if') {
        if (lookup(n.getAttribute('value').match(ONE)[1], scope)) renderInto(parent, Array.from(n.childNodes), scope);
        return;
      }
      var el = n.cloneNode(false), deferred = [];
      Array.from(n.attributes).forEach(function (a) {
        var m = a.value.match(ONE);
        if (/^on/i.test(a.name)) {
          el.removeAttribute(a.name);
          var fn = m && lookup(m[1], scope);
          if (typeof fn === 'function') el.addEventListener(a.name.slice(2).toLowerCase(), fn);
        } else if (a.name === 'checked' && m) {
          el.removeAttribute('checked'); el.checked = !!lookup(m[1], scope);
        } else if (a.name === 'value' && m && (tag === 'select' || tag === 'input')) {
          var v = lookup(m[1], scope); deferred.push(function () { el.value = v == null ? '' : v; });
        } else if (a.value.indexOf('{{') >= 0) {
          el.setAttribute(a.name, interp(a.value, scope));
        }
      });
      var src = tag === 'template' ? n.content : n;
      renderInto(el, Array.from(src.childNodes), scope);
      deferred.forEach(function (f) { f(); });
      parent.appendChild(el);
    });
  }
  document.addEventListener('DOMContentLoaded', function () {
    var host = document.querySelector('x-dc');
    var script = document.querySelector('script[data-dc-script]');
    var meta = JSON.parse(script.getAttribute('data-props') || '{}');
    var q = new URLSearchParams(location.search), props = {};
    Object.keys(meta).forEach(function (k) {
      if (k[0] === '$') return;
      var d = meta[k], raw = q.get(k);
      if (raw == null) props[k] = d.default;
      else if (d.editor === 'boolean') props[k] = raw === '1' || raw === 'true';
      else if (d.editor === 'number') props[k] = Number(raw);
      else props[k] = raw;
    });
    var Component = new Function('DCLogic', script.textContent + '\nreturn Component;')(window.DCLogic);
    var comp = new Component(props);
    var tpl = document.createElement('div');
    tpl.innerHTML = host.innerHTML;
    var helmet = tpl.querySelector('helmet');
    if (helmet) { Array.from(helmet.childNodes).forEach(function (c) { document.head.appendChild(c.cloneNode(true)); }); helmet.remove(); }
    var nodes = Array.from(tpl.childNodes);
    window.__dcRender = function () {
      host.textContent = '';
      renderInto(host, nodes, comp.renderVals());
    };
    window.__dcRender();
  });
})();
