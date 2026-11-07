<!-- Mobile: a phone's on-screen keyboard, followed (menger v1's keyboard.rs, APP-56/58/62,
     CHAT-42, and tapkb.rs). A module only: `follow()` starts it while something that
     needs it is mounted (sc-thread), and returns the stop.

     iOS lays the keyboard over the page without shrinking the layout viewport: the
     *visual* viewport shrinks, and iOS pans it to centre the focused field. Android
     (and `interactive-widget=resizes-content`) shrinks the layout viewport instead.
     So while a field that takes typing has focus and the visible area is ≥ 120px
     short of the page, `<html>` carries `data-kb` and three lengths:
       --kb-top   the visible area's top, in the layout viewport (a fixed view's)
       --kb-h     the visible area's height
       --kb-inset from the layout viewport's bottom up to the keyboard's top
     and a pinned view (sc-thread on a phone) fills top/height, its composer on the
     keyboard. Without the keyboard all of it comes off.

     - The layout height is read off a fixed probe (#sc-kbprobe, top 0 bottom 0),
       never innerHeight: iOS reports innerHeight as the visual viewport's.
     - Something moved the viewport: it is read again every frame for 30 frames, so
       the view tracks iOS's pan as it runs.
     - A scroll iOS makes to centre the field is put back (the page under the
       pinned view stays where it was).
     - The document keeps the height it had when the keyboard came (min-height),
       let go 600ms after the keyboard goes.
     - A pinch-zoomed page (scale > 1.01) is left alone.
     - Taps while the keyboard is up (tapkb.rs): a press on `[data-keep-kb]` (send,
       the way to the latest) keeps the field's focus; any other control runs and
       then the field lets go, so the keyboard goes with it. -->
<script module>
  export const uses = [];

  const KEYBOARD_MIN_PX = 120;
  const MIN_VIEW_PX = 160;
  const FOLLOW_FRAMES = 30;
  const RELEASE_MS = 600;
  const PROBE = "sc-kbprobe";

  /** Whether `el` takes typing (and so raises a keyboard). */
  export function takesTyping(el) {
    if (!el || el.nodeType !== 1) return false;
    const t = el.tagName;
    if (t === "TEXTAREA") return !el.readOnly && !el.disabled;
    if (t === "INPUT") return /^(|text|search|email|url|tel|password|number)$/.test((el.getAttribute("type") || "").toLowerCase()) && !el.readOnly && !el.disabled;
    return !!el.isContentEditable;
  }

  /** The keyboard, if one is up (pure: menger's `keyboard()`). */
  export function keyboard(v) {
    if (!v.editing || v.layoutH <= 0 || v.vvH <= 0 || v.scale > 1.01) return null;
    const overlaid = v.layoutH - v.vvH >= KEYBOARD_MIN_PX;
    const resized = v.tallest - v.layoutH >= KEYBOARD_MIN_PX;
    if (!overlaid && !resized) return null;
    const top = Math.max(0, v.vvTop);
    const height = Math.max(MIN_VIEW_PX, v.vvH);
    return { inset: Math.round(Math.max(0, v.layoutH - top - v.vvH)), top: Math.round(top), height: Math.round(height) };
  }

  /** Whether the keyboard is up now, and a way to hear it change: `onKeyboard(fn)` → stop. */
  export const keyboardUp = () => typeof document !== "undefined" && document.documentElement.hasAttribute("data-kb");
  export function onKeyboard(fn) {
    const on = () => fn(keyboardUp());
    addEventListener("sc-keyboard", on);
    return () => removeEventListener("sc-keyboard", on);
  }

  let users = 0;
  let stopAll = null;
  /** Follows the keyboard while anything that called this is mounted; returns the stop. */
  export function follow() {
    if (typeof window === "undefined" || !window.visualViewport) return () => {};
    if (users++ === 0) stopAll = start();
    let done = false;
    return () => { if (done) return; done = true; if (--users === 0) { stopAll?.(); stopAll = null; } };
  }

  function start() {
    const doc = document;
    const html = doc.documentElement;
    const vv = window.visualViewport;
    let probe = doc.getElementById(PROBE);
    if (!probe) {
      probe = doc.createElement("div");
      probe.id = PROBE;
      probe.setAttribute("aria-hidden", "true");
      probe.style.cssText = "position:fixed;top:0;bottom:0;left:0;width:0;visibility:hidden;pointer-events:none";
      doc.body.append(probe);
    }
    let shown = null;               // the keyboard as last stamped
    let tallest = { w: 0, h: 0 };   // the tallest layout height at this width
    let locked = null;              // the document's scroll while a keyboard is up
    let release = 0;
    let frames = 0;
    let raf = 0;

    const layoutH = () => probe.getBoundingClientRect().height || window.innerHeight;
    const deepActive = () => {
      let a = doc.activeElement;
      while (a?.shadowRoot?.activeElement) a = a.shadowRoot.activeElement;
      return a;
    };

    function apply() {
      const lh = layoutH();
      const w = window.innerWidth;
      tallest = tallest.w === w ? { w, h: Math.max(tallest.h, lh) } : { w, h: lh };
      const kb = keyboard({ layoutH: lh, vvH: vv.height, vvTop: vv.offsetTop, scale: vv.scale, tallest: tallest.h, editing: takesTyping(deepActive()) });
      // A scroll iOS made to centre the field is put back.
      if (kb) {
        if (locked == null) locked = window.scrollY;
        else if (Math.abs(window.scrollY - locked) > 0.5) window.scrollTo(window.scrollX, locked);
      } else if (locked != null) {
        locked = null;
      }
      const same = shown && kb && shown.top === kb.top && shown.height === kb.height && shown.inset === kb.inset;
      if (same || (!shown && !kb)) return;
      const was = shown;
      shown = kb;
      const s = html.style;
      if (kb) {
        if (!was) {
          clearTimeout(release);
          s.minHeight = html.scrollHeight + "px";
        }
        s.setProperty("--kb-inset", kb.inset + "px");
        s.setProperty("--kb-top", kb.top + "px");
        s.setProperty("--kb-h", kb.height + "px");
        html.setAttribute("data-kb", "");
      } else {
        html.removeAttribute("data-kb");
        for (const p of ["--kb-inset", "--kb-top", "--kb-h"]) s.removeProperty(p);
        clearTimeout(release);
        release = setTimeout(() => { if (!shown) s.removeProperty("min-height"); }, RELEASE_MS);
      }
      window.dispatchEvent(new CustomEvent("sc-keyboard", { detail: kb }));
    }

    function tick() {
      raf = 0;
      apply();
      if (--frames > 0) raf = requestAnimationFrame(tick);
    }
    function moved() {
      frames = FOLLOW_FRAMES;
      if (!raf) raf = requestAnimationFrame(tick);
    }

    // A tap while the keyboard is up: send and the way back keep the field's focus;
    // another control acts, then the field lets go.
    function press(ev) {
      if (!html.hasAttribute("data-kb")) return;
      const t = ev.target;
      if (!(t instanceof Element)) return;
      if (t.closest("[data-keep-kb]")) { ev.preventDefault(); return; }
      if (takesTyping(t) || t.closest("label")) return;
      if (!t.closest("button, a[href], summary, [role=button], select, input")) return;
      setTimeout(() => { const a = deepActive(); if (takesTyping(a)) a.blur(); }, 0);
    }

    vv.addEventListener("resize", moved);
    vv.addEventListener("scroll", moved);
    window.addEventListener("resize", moved);
    window.addEventListener("scroll", moved, { passive: true });
    doc.addEventListener("focusin", moved);
    doc.addEventListener("focusout", moved);
    doc.addEventListener("mousedown", press, true);
    moved();

    return () => {
      vv.removeEventListener("resize", moved);
      vv.removeEventListener("scroll", moved);
      window.removeEventListener("resize", moved);
      window.removeEventListener("scroll", moved);
      doc.removeEventListener("focusin", moved);
      doc.removeEventListener("focusout", moved);
      doc.removeEventListener("mousedown", press, true);
      cancelAnimationFrame(raf);
      clearTimeout(release);
      html.removeAttribute("data-kb");
      for (const p of ["--kb-inset", "--kb-top", "--kb-h", "min-height"]) html.style.removeProperty(p);
      probe.remove();
    };
  }
</script>
