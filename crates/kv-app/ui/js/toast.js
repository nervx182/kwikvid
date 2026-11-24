// Error toasts.

const TOAST_MS = 4000;

export function showToast(message) {
  const box = document.getElementById("toasts");
  const el = document.createElement("div");
  el.className = "toast";
  el.dataset.testid = "toast";
  el.textContent = message;
  box.append(el);
  setTimeout(() => el.remove(), TOAST_MS);
}
