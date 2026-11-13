// The server-side JavaScript API of Cosmos DB (stored procedures, triggers, UDFs), written in JavaScript on top of one
// native function, `__host(op, jsonText) -> jsonText`. Loaded once into every script context (see js/mod.rs).
//
// Everything a script hands to the host goes through `plain()`, which measures the value without recursion first, so
// no script can make the native JSON code recurse deeply or run for long. Callbacks of collection calls are queued
// and run by the driver one at a time (`__azemu.step`), like Cosmos DB, which calls them after the calling code returns.
(function (global) {
  "use strict";
  var host = global.__host;
  delete global.__host;
  var MAX_DEPTH = 150;
  var MAX_NODES = 3000000;
  var MAX_ARRAY = 5000000;
  var MAX_STRING = 33554432;   // 32M characters: the longest string repeat / padStart / padEnd / join builds
  var MAX_LOG = 65536;

  function hostCall(op, payload) {
    var r = host(op, payload === undefined ? "" : JSON.stringify(payload));
    return r === "" ? undefined : JSON.parse(r);
  }

  function defineMethod(target, name, fn) {
    Object.defineProperty(target, name, { value: fn, writable: true, configurable: true, enumerable: false });
  }
  var origStringify = JSON.stringify;

  // Check the shape of a value without recursion, then give its JSON text (undefined for undefined).
  function shape(v) {
    var stack = [v, 0];
    var nodes = 0;
    while (stack.length > 0) {
      var d = stack.pop();
      var x = stack.pop();
      if (x !== null && typeof x === "object") {
        if (d > MAX_DEPTH) throw new RangeError("The value is nested too deeply, or it refers to itself.");
        if (++nodes > MAX_NODES) throw new RangeError("The value is too large.");
        if (typeof x.toJSON === "function") continue;
        if (Array.isArray(x)) {
          if (x.length > MAX_ARRAY) throw new RangeError("The array is too large.");
          for (var i = 0; i < x.length; i++) {
            var e = x[i];
            if (e !== null && typeof e === "object") { stack.push(e); stack.push(d + 1); }
          }
        } else {
          var keys = Object.keys(x);
          for (var j = 0; j < keys.length; j++) {
            var p = x[keys[j]];
            if (p !== null && typeof p === "object") { stack.push(p); stack.push(d + 1); }
          }
        }
      }
    }
  }
  function plain(v) {
    if (v === undefined || typeof v === "function" || typeof v === "symbol") return undefined;
    shape(v);
    return origStringify(v);
  }
  // The native JSON.stringify recurses on the structure; the script's own calls get the same check.
  defineMethod(JSON, "stringify", function (v, replacer, space) {
    if (v !== null && typeof v === "object") shape(v);
    return origStringify(v, replacer, space);
  });

  // ---- limits on what one call can build -----------------------------------------------------------------------
  // The engine has no memory limit of its own: these calls build a whole big value in one step.
  function tooBig(what) { return new RangeError("Invalid " + what + " length: a script may build at most " + MAX_STRING + " characters or " + MAX_ARRAY + " elements."); }
  var strProto = String.prototype;
  var arrProto = Array.prototype;
  function guardString(name, lenOf) {
    var orig = strProto[name];
    defineMethod(strProto, name, function () {
      var s = String(this);
      if (lenOf(s, arguments) > MAX_STRING) throw tooBig("string");
      return orig.apply(s, arguments);
    });
  }
  guardString("repeat", function (s, a) { var n = Number(a[0]); return n > 0 ? s.length * n : 0; });
  guardString("padStart", function (s, a) { return Number(a[0]) > 0 ? Number(a[0]) : 0; });
  guardString("padEnd", function (s, a) { return Number(a[0]) > 0 ? Number(a[0]) : 0; });
  guardString("concat", function (s, a) { var n = s.length; for (var i = 0; i < a.length; i++) n += String(a[i]).length; return n; });
  var origFill = arrProto.fill;
  defineMethod(arrProto, "fill", function () {
    if (Number(this.length) > MAX_ARRAY) throw tooBig("array");
    return origFill.apply(this, arguments);
  });
  var origJoin = arrProto.join;
  defineMethod(arrProto, "join", function (sep) {
    var n = Number(this.length);
    if (n > MAX_ARRAY) throw tooBig("array");
    if (sep !== undefined && n > 1 && (n - 1) * String(sep).length > MAX_STRING) throw tooBig("string");
    return origJoin.apply(this, arguments);
  });
  var origConcat = arrProto.concat;
  defineMethod(arrProto, "concat", function () {
    var n = Number(this.length) || 0;
    for (var i = 0; i < arguments.length; i++) n += Array.isArray(arguments[i]) ? arguments[i].length : 1;
    if (n > MAX_ARRAY) throw tooBig("array");
    return origConcat.apply(this, arguments);
  });
  var OrigArray = global.Array;
  var origFrom = OrigArray.from;
  defineMethod(OrigArray, "from", function (src) {
    if (src !== null && src !== undefined && Number(src.length) > MAX_ARRAY) throw tooBig("array");
    return origFrom.apply(OrigArray, arguments);
  });
  // The native `flat` recurses on nested arrays.
  defineMethod(arrProto, "flat", function (depth) {
    var d = depth === undefined ? 1 : Math.floor(Number(depth));
    if (!(d > 0)) d = 0;
    var out = [];
    var arrs = [Object(this)];
    var idx = [0];
    var left = [d];
    while (arrs.length > 0) {
      var t = arrs.length - 1;
      var a = arrs[t];
      var i = idx[t];
      if (i >= (Number(a.length) || 0)) { arrs.pop(); idx.pop(); left.pop(); continue; }
      idx[t] = i + 1;
      if (!(i in a)) continue;
      var e = a[i];
      if (Array.isArray(e) && left[t] > 0) { arrs.push(e); idx.push(0); left.push(left[t] - 1); }
      else { out.push(e); if (out.length > MAX_ARRAY) throw tooBig("array"); }
    }
    return out;
  });

  // The callbacks of these run as ordinary script calls, so the time limit reaches them (a callback called from a
  // native method runs where the clock is not looked at). `thisArg` goes through the native `call` and is not covered.
  function needFn(cb) { if (typeof cb !== "function") throw new TypeError(String(cb) + " is not a function"); }
  function len(o) { var n = Number(o.length); return n >= 0 ? Math.floor(n) : 0; }
  function invoke(cb, thisArg, e, i, o) { return thisArg === undefined ? cb(e, i, o) : cb.call(thisArg, e, i, o); }
  defineMethod(arrProto, "forEach", function (cb, thisArg) {
    var o = Object(this); var n = len(o); needFn(cb);
    for (var i = 0; i < n; i++) if (i in o) invoke(cb, thisArg, o[i], i, o);
  });
  defineMethod(arrProto, "map", function (cb, thisArg) {
    var o = Object(this); var n = len(o); needFn(cb);
    if (n > MAX_ARRAY) throw tooBig("array");
    var out = new Array(n);
    for (var i = 0; i < n; i++) if (i in o) out[i] = invoke(cb, thisArg, o[i], i, o);
    return out;
  });
  defineMethod(arrProto, "filter", function (cb, thisArg) {
    var o = Object(this); var n = len(o); needFn(cb);
    var out = [];
    for (var i = 0; i < n; i++) if (i in o) { var e = o[i]; if (invoke(cb, thisArg, e, i, o)) out.push(e); }
    return out;
  });
  defineMethod(arrProto, "some", function (cb, thisArg) {
    var o = Object(this); var n = len(o); needFn(cb);
    for (var i = 0; i < n; i++) if (i in o && invoke(cb, thisArg, o[i], i, o)) return true;
    return false;
  });
  defineMethod(arrProto, "every", function (cb, thisArg) {
    var o = Object(this); var n = len(o); needFn(cb);
    for (var i = 0; i < n; i++) if (i in o && !invoke(cb, thisArg, o[i], i, o)) return false;
    return true;
  });
  defineMethod(arrProto, "find", function (cb, thisArg) {
    var o = Object(this); var n = len(o); needFn(cb);
    for (var i = 0; i < n; i++) { var e = o[i]; if (invoke(cb, thisArg, e, i, o)) return e; }
    return undefined;
  });
  defineMethod(arrProto, "findIndex", function (cb, thisArg) {
    var o = Object(this); var n = len(o); needFn(cb);
    for (var i = 0; i < n; i++) if (invoke(cb, thisArg, o[i], i, o)) return i;
    return -1;
  });
  defineMethod(arrProto, "reduce", function (cb) {
    var o = Object(this); var n = len(o); needFn(cb);
    var i = 0; var acc;
    if (arguments.length > 1) acc = arguments[1];
    else {
      while (i < n && !(i in o)) i++;
      if (i >= n) throw new TypeError("Reduce of empty array with no initial value");
      acc = o[i++];
    }
    for (; i < n; i++) if (i in o) acc = cb(acc, o[i], i, o);
    return acc;
  });
  defineMethod(arrProto, "reduceRight", function (cb) {
    var o = Object(this); var n = len(o); needFn(cb);
    var i = n - 1; var acc;
    if (arguments.length > 1) acc = arguments[1];
    else {
      while (i >= 0 && !(i in o)) i--;
      if (i < 0) throw new TypeError("Reduce of empty array with no initial value");
      acc = o[i--];
    }
    for (; i >= 0; i--) if (i in o) acc = cb(acc, o[i], i, o);
    return acc;
  });

  // ---- console -------------------------------------------------------------------------------------------------
  var logged = 0;
  function show(a) {
    if (typeof a === "string") return a;
    if (a === undefined) return "undefined";
    if (typeof a === "function") return "[function]";
    try { var s = plain(a); return s === undefined ? String(a) : s; } catch (e) { return "[value]"; }
  }
  function log() {
    var parts = [];
    for (var i = 0; i < arguments.length; i++) parts.push(show(arguments[i]));
    var line = parts.join(" ");
    if (logged < MAX_LOG) {
      logged += line.length + 1;
      host("log", line.length > 4096 ? line.slice(0, 4096) : line);
    }
  }
  var consoleObj = { log: log, info: log, warn: log, error: log, debug: log };
  defineMethod(global, "console", consoleObj);

  // ---- the server-side API -------------------------------------------------------------------------------------
  var cfg = { mode: "sproc", selfLink: "", altLink: "" };
  var queue = [];
  var aborted = null;
  var respBody;
  var respBodySet = false;
  var reqBody;
  var reqBodySet = false;
  var respHeaders = {};
  var reqHeaders = {};

  function mkError(e) {
    var err = new Error(e.message);
    err.number = e.status;
    err.code = e.status;
    err.body = e.body;
    return err;
  }

  function pick(options) {
    var o = {};
    if (options === null || typeof options !== "object") return o;
    var names = ["etag", "ifNoneMatch", "continuation", "pageSize", "disableAutomaticIdGeneration"];
    for (var i = 0; i < names.length; i++) {
      var v = options[names[i]];
      if (v !== undefined && v !== null && typeof v !== "object" && typeof v !== "function") o[names[i]] = v;
    }
    return o;
  }

  function shift(options, callback) {
    if (typeof options === "function") return [{}, options];
    return [options, callback];
  }

  // One collection call: the host does the work now (or refuses: `accepted` false); the callback is queued.
  function call(op, link, bodyText, options, callback) {
    var oc = shift(options, callback);
    var payload = { link: link, options: pick(oc[0]) };
    if (bodyText !== undefined) payload.body = bodyText;
    if (aborted !== null) return false;
    var r = hostCall(op, payload);
    if (!r.accepted) return false;
    var cb = oc[1];
    if (typeof cb === "function") {
      if (r.error) queue.push([cb, [mkError(r.error)]]);
      else queue.push([cb, [undefined, r.resource, r.options || {}]]);
    } else if (r.error) {
      var err = mkError(r.error);
      queue.push([function () { throw err; }, []]);
    }
    return true;
  }

  function needLink(link, what) {
    if (typeof link !== "string") throw new Error("The " + what + " link must be a string.");
  }

  function bodyOf(doc) {
    var t = plain(doc);
    if (t === undefined) throw new Error("The document must be an object.");
    return t;
  }

  var collection = {
    getSelfLink: function () { return cfg.selfLink; },
    getAltLink: function () { return cfg.altLink; },
    createDocument: function (link, body, options, callback) { needLink(link, "collection"); return call("create", link, bodyOf(body), options, callback); },
    upsertDocument: function (link, body, options, callback) { needLink(link, "collection"); return call("upsert", link, bodyOf(body), options, callback); },
    readDocument: function (link, options, callback) { needLink(link, "document"); return call("read", link, undefined, options, callback); },
    replaceDocument: function (link, doc, options, callback) { needLink(link, "document"); return call("replace", link, bodyOf(doc), options, callback); },
    deleteDocument: function (link, options, callback) { needLink(link, "document"); return call("delete", link, undefined, options, callback); },
    readDocuments: function (link, options, callback) { needLink(link, "collection"); return call("readFeed", link, undefined, options, callback); },
    queryDocuments: function (link, filterQuery, options, callback) {
      needLink(link, "collection");
      var q;
      if (typeof filterQuery === "string") q = JSON.stringify({ query: filterQuery });
      else if (filterQuery !== null && typeof filterQuery === "object") q = plain({ query: filterQuery.query, parameters: filterQuery.parameters });
      else throw new Error("The query must be a string or an object with a query text and parameters.");
      return call("query", link, q, options, callback);
    }
  };

  // ---- JavaScript query API (`__.filter`, `__.chain()...value()`) ----------------------------------------------
  function flattenInto(out, arr, shallow, top) {
    for (var i = 0; i < arr.length; i++) {
      var e = arr[i];
      if (Array.isArray(e) && (!shallow || top)) {
        if (shallow) { for (var j = 0; j < e.length; j++) out.push(e[j]); }
        else flattenInto(out, e, false, false);
      } else out.push(e);
    }
  }
  function compare(a, b) { return a < b ? -1 : (a > b ? 1 : 0); }
  function applyOp(items, op) {
    var out = [];
    var i;
    switch (op.t) {
      case "filter":
        for (i = 0; i < items.length; i++) if (op.f(items[i])) out.push(items[i]);
        return out;
      case "map":
        for (i = 0; i < items.length; i++) out.push(op.f(items[i]));
        return out;
      case "pluck":
        for (i = 0; i < items.length; i++) out.push(items[i] === null || items[i] === undefined ? undefined : items[i][op.f]);
        return out;
      case "flatten":
        flattenInto(out, items, op.f === true, true);
        return out;
      case "sortBy":
      case "sortByDescending":
        var keyed = [];
        for (i = 0; i < items.length; i++) keyed.push({ k: op.f === undefined ? items[i] : op.f(items[i]), v: items[i], i: i });
        var sign = op.t === "sortBy" ? 1 : -1;
        keyed.sort(function (x, y) { var c = compare(x.k, y.k); return c !== 0 ? c * sign : x.i - y.i; });
        for (i = 0; i < keyed.length; i++) out.push(keyed[i].v);
        return out;
      case "unwind":
        for (i = 0; i < items.length; i++) {
          var inner = op.f(items[i]);
          if (!Array.isArray(inner)) continue;
          for (var j = 0; j < inner.length; j++) out.push(op.g ? op.g(items[i], inner[j]) : [items[i], inner[j]]);
        }
        return out;
    }
    return items;
  }
  function runChain(ops, options, callback) {
    var oc = shift(options, callback);
    var opts = pick(oc[0]);
    if (aborted !== null) return { isAccepted: false };
    var r = hostCall("readAll", {});
    if (!r.accepted) return { isAccepted: false };
    var items = r.docs;
    for (var i = 0; i < ops.length; i++) items = applyOp(items, ops[i]);
    var size = typeof opts.pageSize === "number" && opts.pageSize > 0 ? Math.floor(opts.pageSize) : 100;
    var skip = 0;
    if (typeof opts.continuation === "string" && opts.continuation.indexOf("azemu-chain:") === 0) skip = parseInt(opts.continuation.slice(12), 10) || 0;
    var page = items.slice(skip, skip + size);
    var next = skip + size < items.length ? "azemu-chain:" + (skip + size) : undefined;
    var cb = oc[1];
    if (typeof cb === "function") {
      queue.push([cb, [undefined, page, next === undefined ? {} : { continuation: next }]]);
    } else {
      respBody = page;
      respBodySet = true;
    }
    return { isAccepted: true };
  }
  function Chain(ops) { this._ops = ops; }
  function addOp(self, t, f, g) { self._ops.push({ t: t, f: f, g: g }); return self; }
  Chain.prototype.filter = function (f) { return addOp(this, "filter", f); };
  Chain.prototype.map = function (f) { return addOp(this, "map", f); };
  Chain.prototype.pluck = function (name) { return addOp(this, "pluck", name); };
  Chain.prototype.flatten = function (shallow) { return addOp(this, "flatten", shallow); };
  Chain.prototype.sortBy = function (f) { return addOp(this, "sortBy", f); };
  Chain.prototype.sortByDescending = function (f) { return addOp(this, "sortByDescending", f); };
  Chain.prototype.unwind = function (f, g) { return addOp(this, "unwind", f, typeof g === "function" ? g : undefined); };
  Chain.prototype.value = function (options, callback) { return runChain(this._ops, options, callback); };
  collection.chain = function () { return new Chain([]); };
  function direct(t) {
    return function (f, a, b, c) {
      if (t === "flatten") {
        // flatten([isShallow], [options], [callback])
        var shallow = typeof f === "boolean" ? f : undefined;
        var rest = typeof f === "boolean" ? [a, b] : [f, a];
        return runChain([{ t: t, f: shallow }], rest[0], rest[1]);
      }
      if (t === "unwind") {
        var g = typeof a === "function" ? a : undefined;
        return runChain([{ t: t, f: f, g: g }], g ? b : a, g ? c : b);
      }
      return runChain([{ t: t, f: f }], a, b);
    };
  }
  var names = ["filter", "map", "pluck", "flatten", "sortBy", "sortByDescending", "unwind"];
  for (var n = 0; n < names.length; n++) collection[names[n]] = direct(names[n]);

  // ---- request, response, context ------------------------------------------------------------------------------
  var request = {
    getBody: function () { return reqBody; },
    setBody: function (v) {
      if (cfg.mode !== "pre") throw new Error("The request body can only be changed in a pre-trigger.");
      plain(v);
      reqBody = v;
      reqBodySet = true;
    },
    getOperationType: function () { return cfg.operation; },
    getValue: function (key) { var h = reqHeaders[String(key).toLowerCase()]; return h === undefined ? undefined : h; },
    setValue: function (key, value) { if (reqHeaders[String(key).toLowerCase()] !== undefined) reqHeaders[String(key).toLowerCase()] = String(value); }
  };
  var response = {
    getBody: function () { return respBody; },
    setBody: function (v) {
      plain(v);
      respBody = v;
      respBodySet = true;
    },
    appendBody: function (s) {
      var add = typeof s === "string" ? s : String(s);
      var cur = respBody === undefined ? "" : (typeof respBody === "string" ? respBody : JSON.stringify(respBody));
      if (cur.length + add.length > MAX_STRING) throw tooBig("string");
      respBody = cur + add;
      respBodySet = true;
    },
    getValue: function (key) { return respHeaders[String(key).toLowerCase()]; },
    setValue: function (key, value) { if (respHeaders[String(key).toLowerCase()] !== undefined) respHeaders[String(key).toLowerCase()] = String(value); },
    getMaxResourceQuota: function () { return "documentSize=10240;documentsSize=10485760;documentsCount=-1;collectionSize=10485760;"; },
    getResourceQuotaCurrentUsage: function () { return "documentSize=0;documentsSize=0;documentsCount=0;collectionSize=0;"; }
  };
  var context = {
    getCollection: function () { return collection; },
    getRequest: function () { return request; },
    getResponse: function () {
      if (cfg.mode === "pre") throw new Error("The response is not available in a pre-trigger.");
      return response;
    },
    abort: function (err) {
      if (aborted === null) aborted = { value: err };
      throw new Error("The script was aborted.");
    }
  };
  Object.defineProperty(global, "getContext", { value: function () { return context; }, writable: true, configurable: true, enumerable: false });
  Object.defineProperty(global, "__", { value: collection, writable: true, configurable: true, enumerable: false });

  // ---- what the driver calls -----------------------------------------------------------------------------------
  var udfs = {};
  function describe(v) {
    try {
      if (v === null || v === undefined) return String(v);
      if (typeof v === "string") return v;
      if (typeof v === "object" && typeof v.message === "string" && (v instanceof Error || typeof v.name === "string")) {
        var nm = typeof v.name === "string" && v.name !== "" ? v.name : "Error";
        return nm + ": " + v.message;
      }
      var s = plain(v);
      return s === undefined ? String(v) : s;
    } catch (e) {
      return "an exception that cannot be shown";
    }
  }
  var api = {
    init: function (c) {
      cfg = c;
      if (c.requestBodyJson !== undefined) { reqBody = JSON.parse(c.requestBodyJson); }
      if (c.responseBodyJson !== undefined) { respBody = JSON.parse(c.responseBodyJson); }
      reqHeaders = c.requestHeaders || {};
      respHeaders = c.responseHeaders || {};
    },
    start: function (fn, args) {
      if (typeof fn !== "function") throw new Error("The script body must be a function.");
      return fn(...args);
    },
    // Run the next queued callback; false when none is left.
    step: function () {
      if (aborted !== null || queue.length === 0) return false;
      var t = queue.shift();
      t[0](...t[1]);
      return true;
    },
    pending: function () { return aborted === null && queue.length > 0; },
    aborted: function () { return aborted === null ? undefined : describe(aborted.value); },
    describe: describe,
    result: function () {
      var out = { bodySet: respBodySet, requestBodySet: reqBodySet };
      if (respBodySet) { var t = plain(respBody); if (t !== undefined) out.body = t; }
      if (reqBodySet) { var u = plain(reqBody); if (u !== undefined) out.requestBody = u; }
      return JSON.stringify(out);
    },
    defineUdf: function (name, fn) {
      if (typeof fn !== "function") throw new Error("The body of the user-defined function '" + name + "' must be a function.");
      udfs[name] = fn;
    },
    callUdf: function (name, argsJson, undefinedJson) {
      var fn = udfs[name];
      if (fn === undefined) throw new Error("The user-defined function '" + name + "' was not found.");
      var args = JSON.parse(argsJson);
      var holes = JSON.parse(undefinedJson);
      for (var i = 0; i < holes.length; i++) args[holes[i]] = undefined;
      var r = fn(...args);
      if (typeof r === "number" && !isFinite(r)) return "";
      var t = plain(r);
      return t === undefined ? "" : t;
    }
  };
  Object.defineProperty(global, "__azemu", { value: api, writable: false, configurable: false, enumerable: false });
})(globalThis);
