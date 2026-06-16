(function () {
  const PIXI = window.PIXI;
  const mount = document.getElementById("pixi-hero-layer");

  if (!PIXI || !mount) {
    return;
  }

  const particles = [];
  const burstParticles = [];
  let app = null;
  let running = true;

  function randomBetween(min, max) {
    return min + Math.random() * (max - min);
  }

  function makeGlowDot(radius, color, alpha) {
    const dot = new PIXI.Graphics();
    dot.circle(0, 0, radius);
    dot.fill({ color, alpha });
    return dot;
  }

  function resetParticle(particle, width, height) {
    particle.x = randomBetween(82, 285);
    particle.y = randomBetween(height - 138, height - 42);
    particle.baseY = particle.y;
    particle.life = randomBetween(0, Math.PI * 2);
    particle.speed = randomBetween(0.012, 0.026);
    particle.drift = randomBetween(0.08, 0.24);
    particle.alpha = randomBetween(0.22, 0.6);
    particle.scale.set(randomBetween(0.5, 1.18));
  }

  function addParticleLayer(stage, width, height) {
    const layer = new PIXI.Container();
    stage.addChild(layer);

    for (let index = 0; index < 28; index += 1) {
      const particle = makeGlowDot(randomBetween(1.1, 2.8), 0xffc23a, randomBetween(0.35, 0.82));
      particle.blendMode = "add";
      resetParticle(particle, width, height);
      particles.push(particle);
      layer.addChild(particle);
    }
  }

  function emitPlayBurst() {
    if (!app) return;

    const width = app.renderer.width;
    const height = app.renderer.height;

    for (let index = 0; index < 18; index += 1) {
      const particle = makeGlowDot(randomBetween(1.7, 4.8), 0xffd86b, randomBetween(0.42, 0.9));
      particle.blendMode = "add";
      particle.x = randomBetween(96, 270);
      particle.y = randomBetween(height - 118, height - 58);
      particle.vx = randomBetween(-1.1, 1.35);
      particle.vy = randomBetween(-1.9, -0.55);
      particle.life = 1;
      particle.scale.set(randomBetween(0.65, 1.3));
      burstParticles.push(particle);
      app.stage.addChild(particle);
    }
  }

  function updateParticles(deltaTime) {
    if (!running || !app) return;

    const width = app.renderer.width;
    const height = app.renderer.height;

    for (const particle of particles) {
      particle.life += particle.speed * deltaTime;
      particle.x += particle.drift * deltaTime;
      particle.y = particle.baseY + Math.sin(particle.life) * 8;
      particle.alpha = 0.24 + Math.sin(particle.life * 1.45) * 0.18;

      if (particle.x > 315 || particle.y < height - 170 || particle.y > height - 20) {
        resetParticle(particle, width, height);
        particle.x = randomBetween(72, 130);
      }
    }

    for (let index = burstParticles.length - 1; index >= 0; index -= 1) {
      const particle = burstParticles[index];
      particle.life -= 0.025 * deltaTime;
      particle.x += particle.vx * deltaTime;
      particle.y += particle.vy * deltaTime;
      particle.vy += 0.018 * deltaTime;
      particle.alpha = Math.max(0, particle.life);
      particle.scale.x += 0.01 * deltaTime;
      particle.scale.y = particle.scale.x;

      if (particle.life <= 0) {
        particle.parent.removeChild(particle);
        particle.destroy();
        burstParticles.splice(index, 1);
      }
    }
  }

  async function start() {
    app = new PIXI.Application();
    await app.init({
      resizeTo: mount,
      backgroundAlpha: 0,
      antialias: true,
      autoDensity: true,
      resolution: Math.min(window.devicePixelRatio || 1, 2),
      preference: "webgl",
      powerPreference: "low-power",
    });

    mount.appendChild(app.canvas);
    addParticleLayer(app.stage, app.renderer.width, app.renderer.height);

    app.ticker.add((ticker) => {
      updateParticles(ticker.deltaTime || 1);
    });

    window.addEventListener("aqw:play-clicked", emitPlayBurst);
    window.aqwPixiLayer = {
      app,
      emitPlayBurst,
      setEnabled(enabled) {
        running = Boolean(enabled);
        mount.style.opacity = running ? "0.78" : "0";
      },
    };
  }

  start().catch(() => {
    mount.remove();
  });
}());
