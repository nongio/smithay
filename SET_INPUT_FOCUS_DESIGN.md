# Design: explicit X11 focus primitives for `X11Surface` (`set_input_focus` / `offer_focus`)

## Problem

smithay's `KeyboardTarget for X11Surface` (`src/xwayland/xwm/surface.rs`) bundles the **X11 focus
protocol** with **`wl_keyboard` delivery**, and picks the protocol internally from the window's ICCCM
input model:

```rust
let (set_input_focus, send_take_focus) = match self.input_model() {
    WmInputModel::None          => return,         // no focus, no keys
    WmInputModel::Passive       => (true,  false), // XSetInputFocus
    WmInputModel::LocallyActive => (true,  true),  // XSetInputFocus + WM_TAKE_FOCUS
    WmInputModel::GloballyActive=> (false, true),  // WM_TAKE_FOCUS only
};
```

This is ICCCM-correct, but it is the **only** way a smithay-based WM can drive X11 focus, and the policy
is not overridable. In particular there is **no way to force the X11 input focus on a Globally-Active
window without sending `WM_TAKE_FOCUS`**.

That matters for **Globally-Active** clients (`WM_HINTS.input == false` + `WM_TAKE_FOCUS`) that mishandle
`WM_TAKE_FOCUS` — most importantly **Unity / Proton (DXVK) games** (e.g. Cuphead). Their winex11/Wine
`WM_TAKE_FOCUS` handler re-asserts fullscreen / issues a mode-set that **stalls the render loop**, so the
ICCCM-correct "offer focus" path is unusable: the game renders but freezes, or never takes focus.

The reference compositor for this workload, **gamescope**, sidesteps the whole ICCCM dance and force-sets
the X input focus directly:

```c
// gamescope src/steamcompmgr.cpp ~4857 — WM_HINTS.input only gates XRaiseWindow, not focus
XSetInputFocus(ctx->dpy, w->xwayland().id, RevertToNone, CurrentTime);
// grep WM_TAKE_FOCUS steamcompmgr.cpp  ->  nothing
```

A smithay-based compositor cannot express that today.

## Comparison

| Compositor | Globally-Active focus | API shape | Cuphead-class games |
|---|---|---|---|
| **smithay** (today) | `WM_TAKE_FOCUS` only, chosen internally by `enter` | one bundled `KeyboardTarget::enter`; no override | ❌ `WM_TAKE_FOCUS` stalls render |
| **wlroots** | `WM_TAKE_FOCUS` only (`offer_focus`); `activate` forces focus | **two explicit fns** the compositor chooses between | ✅ compositor can force focus |
| **gamescope** | bare `XSetInputFocus(RevertToNone)`, no `WM_TAKE_FOCUS` | force focus always | ✅ |

wlroots is the precedent for the right shape:
- `wlr_xwayland_surface_activate()` → `xcb_set_input_focus()` (force).
- `wlr_xwayland_surface_offer_focus()` → `WM_TAKE_FOCUS` only ("it will get focus by itself").
- `wlr_xwayland_icccm_input_model()` → classify, so the compositor decides which to call.

## Chosen design

**Expose the two X11 focus *primitives* as public methods on `X11Surface`, mirroring wlroots' proven
split, and leave `KeyboardTarget::enter`/`leave` unchanged (back-compat).** The compositor keeps the
existing bundled `enter` as the ICCCM default, or opts into its own policy with the primitives.

Considered and rejected:
- *(a) only `set_input_focus`* (what we shipped first): solves the gamescope case but is asymmetric —
  there's a public "force" with no public "offer", so a WM can't implement the full wlroots policy itself.
- *(c) a focus-policy enum on `enter`*: changes the existing signature/behavior, bigger blast radius,
  and still couples focus with `wl_keyboard` delivery. Worse for back-compat and composability.

Naming: `set_input_focus` matches smithay's `set_*` convention and avoids colliding with the existing
`set_activated` (which manages `_NET_WM_STATE_FOCUSED`, a different concept). `offer_focus` matches the
widely-understood wlroots name and the ICCCM verb.

### Signatures (in `impl X11Surface`)

```rust
/// Directly set the X11 input focus to this window (`XSetInputFocus`, RevertToNone),
/// WITHOUT sending `WM_TAKE_FOCUS`. The "force focus" primitive (cf.
/// `wlr_xwayland_surface_activate`). Needed for Globally-Active clients that mishandle
/// WM_TAKE_FOCUS (Unity/Proton games, gamescope). Does not deliver wl_keyboard events —
/// set the seat keyboard focus separately.
pub fn set_input_focus(&self) -> Result<(), ConnectionError>;

/// Offer keyboard focus via a `WM_TAKE_FOCUS` client message; the client sets the
/// focus itself (ICCCM §4.1.7). The "offer focus" primitive (cf.
/// `wlr_xwayland_surface_offer_focus`). The ICCCM-correct path for Globally/Locally-Active
/// clients. Does not deliver wl_keyboard events.
pub fn offer_focus(&self) -> Result<(), ConnectionError>;
```

Both are thin wrappers over the existing connection calls already used privately in `enter`
(`conn.set_input_focus(InputFocus::NONE, window, CURRENT_TIME)` and the `WM_TAKE_FOCUS`
`ClientMessageEvent`), so there is no new protocol surface — only new *access* to it.

### Notes / decisions
- **RevertTo:** fixed to `InputFocus::NONE` (`RevertToNone`), matching gamescope and smithay's own
  internal `enter` call. A `revert_to` parameter could be added later if a compositor needs
  `PointerRoot` semantics; omitted now to keep the API minimal.
- **No `wl_keyboard` delivery:** these set X11 focus only. The compositor still calls
  `KeyboardHandle::set_focus(surface)` so XWayland actually receives keys (XWayland delivers to the
  X-input-focused window; both are required).
- **Back-compat:** purely additive. `enter`/`leave` and `input_model` are untouched; existing WMs are
  unaffected. (`enter` *could* later be refactored to call these two internally for DRY, but that's a
  separate, behavior-preserving change.)
- **A future `unset_input_focus()`** (the `leave` behavior: `set_input_focus(None-window)`) is a natural
  companion if a use case appears; not needed by current consumers (focus is retained on the game).

## Upstream PR

**Title:** `xwm: expose explicit X11 focus primitives (set_input_focus / offer_focus)`

**Description:**
> `KeyboardTarget for X11Surface` bundles the X11 focus protocol with `wl_keyboard` delivery and chooses
> the protocol internally from the ICCCM input model. There is currently no way for a compositor to drive
> X11 focus explicitly — in particular, no way to *force* the input focus on a Globally-Active window
> without sending `WM_TAKE_FOCUS`.
>
> This blocks running Globally-Active clients that mishandle `WM_TAKE_FOCUS` — notably Unity/Proton (DXVK)
> games such as Cuphead, whose `WM_TAKE_FOCUS` handler re-asserts fullscreen and stalls their render loop.
> gamescope handles these by force-setting the input focus with bare `XSetInputFocus` and never sending
> `WM_TAKE_FOCUS`.
>
> This PR adds two public `X11Surface` methods mirroring wlroots' `activate` / `offer_focus` split:
> - `set_input_focus()` — `XSetInputFocus` (RevertToNone), no `WM_TAKE_FOCUS` ("force focus").
> - `offer_focus()` — send `WM_TAKE_FOCUS` only ("offer focus", ICCCM-correct for Globally/Locally-Active).
>
> Both wrap calls already made privately in `KeyboardTarget::enter`; `enter`/`leave` behavior is unchanged
> (purely additive). They affect only X11 focus, not `wl_keyboard` delivery — the compositor still sets the
> seat keyboard focus. This lets a smithay WM implement its own focus policy (e.g. force-focus fullscreen
> games like gamescope, offer-focus everything else like wlroots).

**Commit message:**
```
xwm: expose explicit X11 focus primitives on X11Surface

Add X11Surface::set_input_focus() (force input focus, XSetInputFocus
RevertToNone, no WM_TAKE_FOCUS) and X11Surface::offer_focus() (send only
WM_TAKE_FOCUS), mirroring wlroots' activate/offer_focus split.

KeyboardTarget::enter previously bundled the X11 focus protocol with
wl_keyboard delivery and chose the protocol internally from the ICCCM input
model, with no override. That makes Globally-Active clients which mishandle
WM_TAKE_FOCUS (Unity/Proton DXVK games e.g. Cuphead; anything under gamescope)
unrunnable: WM_TAKE_FOCUS stalls their render loop. gamescope force-sets the
input focus instead.

The new methods wrap calls already used privately in enter, so this is purely
additive and back-compat (enter/leave unchanged). They set only the X11 input
focus, not wl_keyboard focus.
```
