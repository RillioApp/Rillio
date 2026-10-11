// A route's gamepad handlers only sit on the app's gamepad while that route
// is the top one. The fake below dispatches like GamepadProvider does: the
// last handler registered for an event wins.

const { createGamepadScope } = require('../src/services/GamepadContext/gamepadScope');

const fakeGamepad = () => {
    const handlers = new Map();
    return {
        on: (event, id, callback) => {
            if (!handlers.has(event)) handlers.set(event, new Map());
            handlers.get(event).set(id, callback);
        },
        off: (event, id) => {
            const forEvent = handlers.get(event);
            if (forEvent) forEvent.delete(id);
        },
        emit: (event, data) => {
            const latest = Array.from((handlers.get(event) || new Map()).values()).slice(-1)[0];
            if (latest) latest(data);
        },
        ids: (event) => Array.from((handlers.get(event) || new Map()).keys()),
    };
};

describe('createGamepadScope', () => {
    it('registers nothing until the route is live, then everything in order', () => {
        const gamepad = fakeGamepad();
        const scope = createGamepadScope(gamepad.on, gamepad.off);
        scope.on('analog', 'nav', jest.fn());
        scope.on('analog', 'content', jest.fn());
        expect(gamepad.ids('analog')).toEqual([]);
        scope.setLive(true);
        expect(gamepad.ids('analog')).toEqual(['nav', 'content']);
    });

    it('takes a covered route off the gamepad, also for events the top route does not bind', () => {
        const gamepad = fakeGamepad();
        const below = createGamepadScope(gamepad.on, gamepad.off);
        const top = createGamepadScope(gamepad.on, gamepad.off);
        const tabs = jest.fn();
        const select = jest.fn();
        const playerSelect = jest.fn();

        below.setLive(true);
        below.on('buttonRT', 'board', tabs);
        below.on('buttonA', 'metadetails', select);
        below.setLive(false);
        top.setLive(true);
        top.on('buttonA', 'player', playerSelect);

        gamepad.emit('buttonRT');
        gamepad.emit('buttonA');
        expect(tabs).not.toHaveBeenCalled();
        expect(select).not.toHaveBeenCalled();
        expect(playerSelect).toHaveBeenCalledTimes(1);
    });

    it('keeps a covered route off the gamepad when one of its effects re-registers', () => {
        const gamepad = fakeGamepad();
        const below = createGamepadScope(gamepad.on, gamepad.off);
        const top = createGamepadScope(gamepad.on, gamepad.off);
        const back = jest.fn();
        const playerBack = jest.fn();

        below.setLive(true);
        below.on('buttonB', 'board', back);
        below.setLive(false);
        top.setLive(true);
        top.on('buttonB', 'player-top-bar', playerBack);
        // The effect cleanup + setup a dependency change causes.
        below.off('buttonB', 'board');
        below.on('buttonB', 'board', back);

        gamepad.emit('buttonB');
        expect(back).not.toHaveBeenCalled();
        expect(playerBack).toHaveBeenCalledTimes(1);
    });

    it('leaves another route\'s handler alone when a covered route drops the same id', () => {
        const gamepad = fakeGamepad();
        const below = createGamepadScope(gamepad.on, gamepad.off);
        const top = createGamepadScope(gamepad.on, gamepad.off);
        const topBack = jest.fn();

        below.setLive(true);
        below.on('buttonB', 'shared', jest.fn());
        below.setLive(false);
        top.setLive(true);
        top.on('buttonB', 'shared', topBack);
        below.off('buttonB', 'shared');

        gamepad.emit('buttonB');
        expect(topBack).toHaveBeenCalledTimes(1);
    });

    it('gives the handlers back when the route is on top again, minus the ones dropped meanwhile', () => {
        const gamepad = fakeGamepad();
        const scope = createGamepadScope(gamepad.on, gamepad.off);
        const select = jest.fn();

        scope.setLive(true);
        scope.on('buttonA', 'metadetails', select);
        scope.on('buttonX', 'gone', jest.fn());
        scope.setLive(false);
        scope.off('buttonX', 'gone');
        scope.setLive(true);

        gamepad.emit('buttonA');
        expect(select).toHaveBeenCalledTimes(1);
        expect(gamepad.ids('buttonX')).toEqual([]);
    });
});
