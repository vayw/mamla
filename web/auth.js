const el = (id) => document.getElementById(id);
let invitation = new URLSearchParams(location.hash.slice(1)).get('invite');
// The invite stays out of HTTP requests, access logs, Referer and copied room URLs.
if (invitation) history.replaceState(null, '', location.pathname + location.search);
let allowed = false;
let enabled = false;
let personalKey = null;

function notice(text) {
  el(el('access-panel').hidden ? 'message' : 'access-message').textContent = text;
}
function loginScreen() {
  allowed = false;
  el('access-panel').hidden = false;
  el('room-panel').hidden = true;
  el('service-logout').hidden = true;
  el('invite-panel').hidden = !invitation;
  el('login-generated-key').hidden = false;
}
function roomScreen() {
  allowed = true;
  el('access-panel').hidden = true;
  el('room-panel').hidden = false;
  el('service-logout').hidden = !enabled;
  el('login-generated-key').hidden = true;
  el('access-mode').textContent = enabled ? 'Доступ по приглашению' : 'Без регистрации';
}

async function post(action, body) {
  const response = await fetch(`/api/auth/${action}`, {
    method: 'POST', headers: { 'Content-Type': 'application/json' }, body: JSON.stringify(body),
  });
  if (!response.ok) {
    const body = await response.json().catch(() => ({}));
    throw new Error(body.error || `Сервис ответил ${response.status}`);
  }
}

async function refresh() {
  const response = await fetch('/api/auth/status');
  if (!response.ok) throw new Error('Не удалось проверить доступ к сервису.');
  const status = await response.json();
  enabled = status.enabled;
  if (!enabled || (status.authenticated && !invitation)) roomScreen();
  else loginScreen();
  return status.authenticated;
}

const ready = refresh().catch(() => {
  loginScreen();
  notice('Сервис недоступен. Проверьте соединение, HTTPS-сертификат и обновите страницу.');
  return false;
});
export async function accessAllowed() { await ready; return allowed; }
export function accessExpired() {
  loginScreen();
  notice('Доступ завершён или отозван. Введите личный ключ для входа.');
  window.dispatchEvent(new Event('mamla:access-lost'));
}

async function login(key) {
  await post('login', { key });
  invitation = null;
  if (!await refresh()) throw new Error('Браузер не сохранил сессию. Откройте Mamla по HTTPS.');
  el('access-key').value = '';
  notice('');
}

el('access-form').addEventListener('submit', async (event) => {
  event.preventDefault();
  el('access-login').disabled = true;
  try { await login(el('access-key').value.trim()); }
  catch (error) { notice(error.message || 'Не удалось войти.'); }
  finally { el('access-login').disabled = false; }
});

el('accept-invite').addEventListener('click', async () => {
  el('accept-invite').disabled = true;
  try {
    if (!personalKey) {
      const bytes = crypto.getRandomValues(new Uint8Array(32));
      personalKey = 'mk_' + btoa(String.fromCharCode(...bytes)).replaceAll('+', '-').replaceAll('/', '_').replace(/=+$/, '');
    }
    // Show the generated key before the request. If a successful response is
    // lost, the invite may be spent; this key still lets its owner recover access.
    el('generated-key').value = personalKey;
    el('key-panel').hidden = false;
    el('key-hint').textContent = 'Ключ создан в этом браузере. Сохраните его; подтверждаем приглашение…';
    await post('accept', { invite: invitation, key: personalKey });
    invitation = null;
    if (!await refresh()) throw new Error('Ключ активирован, но браузер не сохранил сессию. Используйте HTTPS и войдите с этим ключом.');
    el('key-hint').textContent = 'Доступ активирован. Сохраните этот ключ — сервер хранит только его хеш и не сможет показать ключ снова.';
    notice('');
  } catch (error) {
    notice(error.message || 'Не удалось принять приглашение.');
    el('key-hint').textContent = 'Если подтверждение оборвалось, сохраните ключ и попробуйте «Войти с этим ключом». Если приглашение не успело активироваться — повторите принятие.';
  } finally { el('accept-invite').disabled = false; }
});

el('login-generated-key').addEventListener('click', async () => {
  el('login-generated-key').disabled = true;
  try { await login(personalKey); }
  catch (error) { notice(error.message || 'Не удалось войти.'); }
  finally { el('login-generated-key').disabled = false; }
});

el('download-key').addEventListener('click', () => {
  if (!personalKey) return;
  const url = URL.createObjectURL(new Blob([personalKey + '\n'], { type: 'text/plain' }));
  const link = document.createElement('a');
  link.href = url;
  link.download = 'mamla-access-key.txt';
  document.body.append(link);
  link.click();
  link.remove();
  setTimeout(() => URL.revokeObjectURL(url), 1000);
});

el('hide-key').addEventListener('click', () => {
  personalKey = null;
  el('generated-key').value = '';
  el('key-panel').hidden = true;
});

el('service-logout').addEventListener('click', async () => {
  el('service-logout').disabled = true;
  window.dispatchEvent(new Event('mamla:access-lost'));
  try {
    await post('logout', {});
    personalKey = null;
    el('generated-key').value = '';
    el('key-panel').hidden = true;
    loginScreen();
    notice('Вы вышли из сервиса.');
  } catch (error) { notice(error.message || 'Не удалось выйти из сервиса.'); }
  finally { el('service-logout').disabled = false; }
});
