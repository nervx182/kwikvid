// Toasts: errors by default; info toasts can carry one action button.

const TOAST_MS = 4000;
const ACTION_TOAST_MS = 8000;

/**
 * Show `message` for a few seconds.
 *
 * options.kind:   "error" (default) or "info".
 * options.action: { label, onClick } adds a button; clicking it runs
 *                 onClick and closes the toast.
 * Returns the toast element.
 */
export function showToast(message, { kind = "error", action = null } = {}) {
  const box = document.getElementById("toasts");
  const el = document.createElement("div");
  el.className = kind === "info" ? "toast info" : "toast";
  el.dataset.testid = "toast";
  const text = document.createElement("span");
  text.textContent = message;
  el.append(text);
  if (action) {
    const btn = document.createElement("button");
    btn.type = "button";
    btn.className = "toast-action";
    btn.dataset.testid = "toast-action";
    btn.textContent = action.label;
    btn.addEventListener("click", () => {
      el.remove();
      action.onClick();
    });
    el.append(btn);
  }
  box.append(el);
  setTimeout(() => el.remove(), action ? ACTION_TOAST_MS : TOAST_MS);
  return el;
}
