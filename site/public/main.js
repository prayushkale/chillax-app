// Hero ad: muted autoplay, with an opt-in sound toggle. Paused when the
// visitor prefers reduced motion.
const ad = document.getElementById('ad');
const sound = document.getElementById('sound');
if (matchMedia('(prefers-reduced-motion: reduce)').matches) {
  ad.removeAttribute('autoplay');
  ad.pause();
  ad.controls = true;
}
sound.addEventListener('click', () => {
  ad.muted = !ad.muted;
  if (!ad.muted) { ad.currentTime = 0; ad.play(); }
  sound.setAttribute('aria-pressed', String(!ad.muted));
  sound.textContent = ad.muted ? 'Turn sound on' : 'Turn sound off';
});

// The two gates: clicking approves, the same words the product uses.
const approved = {
  sprint: 'Approved: SP-1 started, developers picking up tickets',
  release: 'Approved: v0.1.0 merged to main and tagged',
};
document.querySelectorAll('.gate').forEach((gate) => {
  const button = gate.querySelector('button');
  button.addEventListener('click', () => {
    if (gate.classList.contains('approved')) return;
    gate.classList.add('approved');
    gate.querySelector('.state').textContent = approved[gate.dataset.gate];
    button.textContent = 'Approved';
    button.setAttribute('aria-disabled', 'true');
  });
});

// Walkthrough: load YouTube only on click. Until the video is published,
// send people to the GitHub release that hosts the MP4.
const yt = document.getElementById('yt');
yt.querySelector('button').addEventListener('click', () => {
  const id = yt.dataset.id;
  if (!id) {
    location.href = 'https://github.com/prayushkale/chillax-app/releases/latest';
    return;
  }
  yt.innerHTML = `<iframe src="https://www.youtube-nocookie.com/embed/${id}?autoplay=1&rel=0" title="Chillax 5-minute walkthrough" allow="autoplay; encrypted-media; picture-in-picture; fullscreen" allowfullscreen></iframe>`;
});
