// Copyright (C) 2017-2024 Smart code 203358507

/**
 * AddonDetailsModal - clean-room rewrite composed directly on the foundation-kit
 * Dialog (Radix), not on ModalDialog. The manifest is fetched with useAddonDetails
 * and the Install / Uninstall dispatches, usePlatform.openExternal configure flow,
 * remote-vs-local branching and withCoreSuspender gate are reused verbatim.
 *
 * The public props contract is unchanged: `transportUrl` selects the addon and
 * `onCloseRequest({ type, reactEvent?, nativeEvent? })` fires for every dismissal
 * (Escape / outside-click / close button -> 'close'; footer buttons -> their action
 * type). Consumers (Addons.tsx, Discover.tsx) pass only these two props.
 *
 * Footer buttons use proper kit variants instead of LESS-specificity overrides:
 * cancel is `ghost`, configure is `outline`, install is the accent `default`, and
 * uninstall is a destructive-tinted outline. One flat rounded-full row at the kit's
 * h-10 rhythm - no ad-hoc opacity/offset overrides.
 */

import React, { useCallback, useMemo, type ReactNode } from 'react';
import { useTranslation } from 'react-i18next';
import { useCore } from 'rillio/core';
import { usePlatform } from 'rillio/common';
import { withCoreSuspender } from 'rillio/common/CoreSuspender';
import { Dialog, DialogContent, DialogTitle } from 'rillio/components/ui/dialog';
import { Button } from 'rillio/components/ui/button';
import { cn } from 'rillio/components/ui/cn';
import AddonDetails from './AddonDetails';

const useAddonDetails = require('./useAddonDetails').default;

type CloseEvent = {
    type: string;
    reactEvent?: React.SyntheticEvent;
    nativeEvent?: Event;
};

type Props = {
    transportUrl?: string;
    onCloseRequest?: (event: CloseEvent) => void;
};

// Shared surface: content sized to ~36rem, body scrolls, background painted at 0.1.
const DIALOG_CLASS = 'flex w-full max-w-[36rem] flex-col gap-0 overflow-hidden p-0';

const withRemoteAndLocalAddon = (Component: typeof AddonDetails) => {
    const Wrapped = ({ remoteAddon, localAddon }: { remoteAddon: any; localAddon: any }) => {
        const addon = remoteAddon !== null && remoteAddon.content.type === 'Ready' ?
            remoteAddon.content.content
            :
            localAddon !== null ?
                localAddon
                :
                null;
        if (addon === null) {
            return null;
        }
        return (
            <Component
                id={addon.manifest.id}
                name={addon.manifest.name}
                version={addon.manifest.version}
                logo={addon.manifest.logo}
                description={addon.manifest.description}
                types={addon.manifest.types}
                transportUrl={addon.transportUrl}
                official={addon.flags.official}
            />
        );
    };
    Wrapped.displayName = 'withRemoteAndLocalAddon';
    return Wrapped;
};

const AddonDetailsWithRemoteAndLocalAddon = withRemoteAndLocalAddon(AddonDetails);

// The dialog surface, shared by the loaded modal and its suspense fallback.
const AddonDetailsShell = ({ title, background, footer, children }: {
    title: string;
    background?: string | null;
    footer?: ReactNode;
    children: ReactNode;
}) => (
    <DialogContent className={DIALOG_CLASS}>
        {typeof background === 'string' && background.length > 0 ? (
            <div
                className="pointer-events-none absolute inset-0 rounded-squircle bg-cover bg-center opacity-10"
                style={{ backgroundImage: `url('${background}')` }}
            />
        ) : null}
        <div className="relative flex min-h-0 flex-col overflow-y-auto p-6">
            <DialogTitle className="mb-6 pr-8 text-xl font-semibold text-fg">{title}</DialogTitle>
            <div className="min-h-0 flex-1">{children}</div>
            {footer}
        </div>
    </DialogContent>
);

const AddonDetailsModal = ({ transportUrl, onCloseRequest }: Props) => {
    const { t } = useTranslation();
    const core = useCore();
    const platform = usePlatform();
    const addonDetails = useAddonDetails(transportUrl);

    const emitClose = useCallback((type: string, event?: React.SyntheticEvent) => {
        if (typeof onCloseRequest === 'function') {
            onCloseRequest({ type, reactEvent: event, nativeEvent: event?.nativeEvent });
        }
    }, [onCloseRequest]);

    const remoteReady = addonDetails.remoteAddon !== null &&
        addonDetails.remoteAddon.content.type === 'Ready' ?
        addonDetails.remoteAddon.content.content
        :
        null;

    const onCancel = useCallback((event: React.MouseEvent) => {
        emitClose('cancel', event);
    }, [emitClose]);
    const onConfigure = useCallback((event: React.MouseEvent) => {
        platform.openExternal(String(transportUrl).replace('manifest.json', 'configure'));
        emitClose('configure', event);
    }, [platform, transportUrl, emitClose]);
    const onUninstall = useCallback((event: React.MouseEvent) => {
        core.transport.dispatch({
            action: 'Ctx',
            args: { action: 'UninstallAddon', args: addonDetails.localAddon }
        });
        emitClose('uninstall', event);
    }, [core, addonDetails.localAddon, emitClose]);
    const onInstall = useCallback((event: React.MouseEvent) => {
        core.transport.dispatch({
            action: 'Ctx',
            args: { action: 'InstallAddon', args: remoteReady }
        });
        emitClose('install', event);
    }, [core, remoteReady, emitClose]);

    const showConfigure = remoteReady !== null && remoteReady.manifest.behaviorHints.configurable;
    const showUninstall = addonDetails.localAddon !== null;
    const showInstall = !showUninstall && remoteReady !== null &&
        !remoteReady.manifest.behaviorHints.configurationRequired;

    const footer = useMemo(() => (
        <div className="mt-8 flex flex-col-reverse gap-3 sm:flex-row sm:justify-end">
            <Button variant="ghost" className="text-fg-muted hover:text-fg" onClick={onCancel}>
                {t('BUTTON_CANCEL')}
            </Button>
            {showConfigure ? (
                <Button variant="outline" onClick={onConfigure}>
                    {t('ADDON_CONFIGURE')}
                </Button>
            ) : null}
            {showUninstall ? (
                <Button
                    variant="outline"
                    className="border-danger/40 text-danger hover:bg-danger/10 hover:text-danger"
                    onClick={onUninstall}
                >
                    {t('ADDON_UNINSTALL')}
                </Button>
            ) : showInstall ? (
                <Button onClick={onInstall}>
                    {t('ADDON_INSTALL')}
                </Button>
            ) : null}
        </div>
    ), [t, showConfigure, showUninstall, showInstall, onCancel, onConfigure, onUninstall, onInstall]);

    const background = remoteReady !== null ? remoteReady.manifest.background : null;

    let body: ReactNode;
    if (addonDetails.selected === null) {
        body = <div className="py-6 text-center text-fg-muted">{t('ADDON_LOADING_MANIFEST')}</div>;
    } else if (addonDetails.remoteAddon === null || addonDetails.remoteAddon.content.type === 'Loading') {
        body = (
            <div className="py-6 text-center text-fg-muted">
                {t('ADDON_LOADING_MANIFEST_FROM', { origin: addonDetails.selected.transportUrl })}
            </div>
        );
    } else if (addonDetails.remoteAddon.content.type === 'Err' && addonDetails.localAddon === null) {
        body = (
            <div className="py-6 text-center text-fg-muted">
                {t('ADDON_LOADING_MANIFEST_FAILED', { origin: addonDetails.selected.transportUrl })}
                <div className="mt-2 text-sm">{addonDetails.remoteAddon.content.content.message}</div>
            </div>
        );
    } else {
        body = (
            <AddonDetailsWithRemoteAndLocalAddon
                remoteAddon={addonDetails.remoteAddon}
                localAddon={addonDetails.localAddon}
            />
        );
    }

    return (
        <Dialog open onOpenChange={(next: boolean) => { if (!next) emitClose('close'); }}>
            <AddonDetailsShell
                title={t('ADDON_ADDON')}
                background={background}
                footer={footer}
            >
                {body}
            </AddonDetailsShell>
        </Dialog>
    );
};

const AddonDetailsModalFallback = ({ onCloseRequest }: Props) => {
    const { t } = useTranslation();
    const emitClose = useCallback(() => {
        if (typeof onCloseRequest === 'function') {
            onCloseRequest({ type: 'close' });
        }
    }, [onCloseRequest]);
    return (
        <Dialog open onOpenChange={(next: boolean) => { if (!next) emitClose(); }}>
            <AddonDetailsShell title={t('ADDON_ADDON')}>
                <div className="py-6 text-center text-fg-muted">{t('ADDON_LOADING_MANIFEST')}</div>
            </AddonDetailsShell>
        </Dialog>
    );
};

export default withCoreSuspender(AddonDetailsModal, AddonDetailsModalFallback);
