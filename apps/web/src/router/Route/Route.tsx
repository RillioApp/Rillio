// Copyright (C) 2017-2023 Smart code 203358507

import React from 'react';
import { ModalsContainerProvider } from '../ModalsContainerContext';
import { RouteFocusedProvider } from 'rillio/common/useRouteFocused';
import { RouteGamepadProvider } from 'rillio/services/GamepadContext';

type Props = {
    component?: React.ReactNode,
    focused: boolean,
};

// The router keeps earlier routes mounted under the top one (Routes.tsx), so
// THIS is the one place that says a covered route takes no input:
//  - `inert` on its container: nothing inside it (its own modals included)
//    can be focused, tabbed to, clicked or activated. Everything that must
//    stay usable lives outside every route container: the toaster, the app
//    modals and the kit's menus, tooltips and dialogs portal to <body>, and
//    the top route is never inert. Set through the ref, as a property: React
//    18 does not know the attribute (a boolean prop is dropped) and its
//    typings do not carry it.
//  - focus parked inside it (the stream card that opened the player) is
//    released, so Enter/Space cannot re-trigger it from underneath.
//  - gamepad handlers registered inside it are off the gamepad
//    (RouteGamepadProvider).
// `inert` cannot stop a listener on document/window: those ask
// useRouteFocused (the Shortcuts service does it for its users, onShortcut).
const Route = ({ component, focused }: Props) => {
    const containerRef = React.useRef<HTMLDivElement>(null);

    React.useLayoutEffect(() => {
        const container = containerRef.current;
        if (container === null) {
            throw new Error('Route: the route container is not mounted');
        }
        container.inert = !focused;
        const active = document.activeElement;
        if (!focused && active instanceof HTMLElement && container.contains(active)) {
            active.blur();
        }
    }, [focused]);

    return (
        <div ref={containerRef} className={'route-container'}>
            <RouteFocusedProvider value={focused}>
                <RouteGamepadProvider focused={focused}>
                    <ModalsContainerProvider>
                        <div className={'route-content'}>
                            {component}
                        </div>
                    </ModalsContainerProvider>
                </RouteGamepadProvider>
            </RouteFocusedProvider>
        </div>
    );
};

export default Route;
