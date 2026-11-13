// Imperio website behaviour. First-party only; no analytics, no cookies.
(function () {
  'use strict';
  var doc = document;
  doc.documentElement.classList.add('js');

  // --- version + checksum from the signed manifest the app itself reads ---
  var ver = doc.getElementById('ver');
  var sha = doc.getElementById('sha');
  var shaVer = doc.getElementById('shaver');
  fetch('latest.json', { cache: 'no-store' })
    .then(function (r) { if (!r.ok) throw new Error(r.status); return r.json(); })
    .then(function (m) {
      var v = String(m.version || '').replace(/[^0-9A-Za-z.\-]/g, '');
      var h = String(m.sha256 || '').toLowerCase();
      if (ver && v) ver.textContent = 'Version ' + v;
      if (shaVer && v) shaVer.textContent = ' (version ' + v + ')';
      if (sha && /^[0-9a-f]{64}$/.test(h)) sha.textContent = h;
    })
    .catch(function () {
      if (ver) ver.textContent = 'Free download';
      if (sha) sha.textContent = 'See latest.json on this site.';
    });

  // --- sticky nav state + mobile menu ---
  var nav = doc.querySelector('.nav');
  var btn = doc.getElementById('menuBtn');
  var links = doc.getElementById('navLinks');
  function onScroll() { if (nav) nav.classList.toggle('stuck', window.scrollY > 8); }
  window.addEventListener('scroll', onScroll, { passive: true });
  onScroll();
  function setMenu(open) {
    if (!nav || !btn) return;
    nav.classList.toggle('open', open);
    btn.setAttribute('aria-expanded', open ? 'true' : 'false');
    btn.setAttribute('aria-label', open ? 'Close menu' : 'Open menu');
  }
  if (btn && links) {
    btn.addEventListener('click', function () { setMenu(btn.getAttribute('aria-expanded') !== 'true'); });
    links.addEventListener('click', function (e) { if (e.target.closest('a')) setMenu(false); });
    doc.addEventListener('keydown', function (e) {
      if (e.key === 'Escape' && btn.getAttribute('aria-expanded') === 'true') { setMenu(false); btn.focus(); }
    });
    window.addEventListener('resize', function () { if (window.innerWidth > 920) setMenu(false); });
  }

  // --- scroll reveal ---
  var reveals = doc.querySelectorAll('.reveal');
  if ('IntersectionObserver' in window) {
    var io = new IntersectionObserver(function (entries) {
      entries.forEach(function (e) {
        if (e.isIntersecting) { e.target.classList.add('in'); io.unobserve(e.target); }
      });
    }, { threshold: 0.1, rootMargin: '0px 0px -40px 0px' });
    reveals.forEach(function (el) { io.observe(el); });
  } else {
    reveals.forEach(function (el) { el.classList.add('in'); });
  }

  // --- hero mock: type an address, connect, show the result, repeat ---
  var typed = doc.getElementById('typed');
  var field = doc.getElementById('mockAddr');
  var go = doc.getElementById('mockGo');
  var hint = doc.getElementById('mockHint');
  var code = doc.getElementById('mockCode');
  if (!typed || !field || !go || !hint || !code) return;
  var reduce = window.matchMedia('(prefers-reduced-motion: reduce)').matches;
  var addr = '192.168.1.20';
  var placeholder = 'Enter the other computer’s address or ID';
  function sleep(ms) { return new Promise(function (r) { setTimeout(r, ms); }); }
  function connected() {
    go.textContent = 'Session open';
    hint.textContent = 'The remote screen is open in its own window. F11 for full screen.';
    hint.className = 'hint ok';
  }
  async function loop() {
    if (reduce) {
      typed.textContent = addr; field.classList.add('on');
      code.textContent = '739 913'; code.classList.add('on');
      go.className = 'go busy'; connected();
      return;
    }
    for (;;) {
      typed.textContent = placeholder; field.classList.remove('on');
      code.textContent = 'Code (if any)'; code.classList.remove('on');
      go.textContent = 'Connect'; go.className = 'go';
      hint.textContent = 'Type the address shown on the other computer’s Imperio window, then Connect.';
      hint.className = 'hint';
      await sleep(1600);
      typed.textContent = ''; field.classList.add('on');
      for (var i = 0; i < addr.length; i++) { typed.textContent += addr[i]; await sleep(70 + Math.random() * 60); }
      await sleep(400);
      code.textContent = '739 913'; code.classList.add('on');
      await sleep(600);
      go.textContent = 'Connecting…'; go.className = 'go busy';
      hint.textContent = 'Reaching the other computer…';
      await sleep(1200);
      connected();
      await sleep(4200);
    }
  }
  loop();
})();
