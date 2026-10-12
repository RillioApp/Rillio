// Copyright (C) 2017-2026 Smart code 203358507

import React from 'react';
import GamepadContext from './GamepadContext';

const { createGamepadScope } = require('./gamepadScope');

type Props = {
    focused: boolean,
    children?: React.ReactNode,
};

// Wraps one route (router/Route): gamepad handlers registered inside it only
// reach the app's gamepad while the route is the top one. See gamepadScope.js.
const RouteGamepadProvider = ({ focused, children }: Props) => {
    const gamepad = React.useContext(GamepadContext);
    const on = gamepad?.on;
    const off = gamepad?.off;
    const scope = React.useMemo(() => {
        return on !== undefined && off !== undefined ? createGamepadScope(on, off) : null;
    }, [on, off]);

    // Layout effect: live before the route's own (passive) effects register.
    React.useLayoutEffect(() => {
        scope?.setLive(focused);
        return () => scope?.setLive(false);
    }, [scope, focused]);

    const value = React.useMemo(() => {
        return gamepad !== null && scope !== null ? { ...gamepad, on: scope.on, off: scope.off } : null;
    }, [gamepad, scope]);

    return (
        <GamepadContext.Provider value={value}>
            {children}
        </GamepadContext.Provider>
    );
};

export default RouteGamepadProvider;
