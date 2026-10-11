// Copyright (C) 2017-2026 Smart code 203358507

// One route's view of the gamepad. Plain JS so jest loads it directly
// (jest-covered in tests/gamepadScope.spec.js); RouteGamepadProvider.tsx
// hands it to everything inside a route in place of the app's own on/off.
//
// The provider calls the LAST handler registered for an event, so a route
// left mounted under another one stays reachable: for any event the top
// route does not bind, and for every event again as soon as one of its
// effects re-registers. The scope keeps the route's handlers to itself and
// only puts them on the real gamepad while the route is live (the top one).
// Going live re-registers them in their original order, which also makes
// them the latest again.

const createGamepadScope = (on, off) => {
    // event -> (id -> callback), with the provider's own semantics: one
    // handler per id, a repeated id keeps its place.
    const handlers = new Map();
    let live = false;

    return {
        on: (event, id, callback) => {
            if (!handlers.has(event)) {
                handlers.set(event, new Map());
            }
            handlers.get(event).set(id, callback);
            if (live) {
                on(event, id, callback);
            }
        },
        // While not live the id is not ours on the real gamepad (another
        // route may be using the same one), so there is nothing to remove.
        off: (event, id) => {
            const forEvent = handlers.get(event);
            if (forEvent !== undefined) {
                forEvent.delete(id);
            }
            if (live) {
                off(event, id);
            }
        },
        setLive: (next) => {
            if (next === live) {
                return;
            }
            live = next;
            handlers.forEach((forEvent, event) => {
                forEvent.forEach((callback, id) => {
                    live ? on(event, id, callback) : off(event, id);
                });
            });
        },
    };
};

module.exports = { createGamepadScope };
