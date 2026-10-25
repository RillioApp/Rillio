// Copyright (C) 2017-2023 Smart code 203358507

/**
 * Core-init failure screen. Ported onto Tailwind tokens + the kit Button / Dialog.
 * Both recovery actions are kept: Reload (safe) and Clear data. Clearing local
 * storage is a one-click, irreversible data-loss footgun (it is the ONLY copy of the
 * user's profile / library / settings), so it now sits behind an explicit confirm
 * Dialog rather than firing on a single click.
 */

import React from 'react';
import { useTranslation } from 'react-i18next';
import Image from 'rillio/components/Image';
import { removeItem } from 'rillio/common/profileStorage';
import { Dialog, DialogContent, DialogHeader, DialogTitle, DialogDescription, DialogFooter, Button } from 'rillio/components/ui';

// The active profile's buckets (crates/core/src/constants.rs) plus the web-side
// per-profile keys. Clearing is scoped to the ACTIVE profile: a corrupt boot is
// a per-profile problem, and localStorage.clear() would destroy every OTHER
// profile on the device too. `dismissed_events` is a retired bucket nothing
// reads any more; it stays listed so Clear data also removes it from profiles
// written by older versions.
const PROFILE_KEYS = [
    'schema_version', 'profile', 'library', 'library_recent', 'streams',
    'search_history', 'streaming_server_urls', 'notifications', 'calendar',
    'dismissed_events', 'rillio.displayName', 'rillio.syncLog',
    'rillio.preloadPrompt.enabled',
];

type Props = {
    message: string,
};

const Error = ({ message }: Props) => {
    const { t } = useTranslation();
    const [confirmOpen, setConfirmOpen] = React.useState(false);

    const reload = React.useCallback(() => {
        window.location.reload();
    }, []);

    const clearData = React.useCallback(() => {
        PROFILE_KEYS.forEach(removeItem);
        window.location.reload();
    }, []);

    return (
        <div className="relative flex size-full flex-col items-center justify-center gap-4">
            <Image
                className="h-48 w-48 flex-none object-contain object-center opacity-90"
                src={require('/assets/images/empty.svg')}
                alt={' '}
            />
            <div className="flex flex-col items-center justify-center gap-2">
                <div className="text-[2rem] text-fg">
                    {t('GENERIC_ERROR_MESSAGE')}
                </div>
                <div className="line-clamp-5 max-w-[40rem] text-center text-2xl text-fg-muted">
                    {message}
                </div>
            </div>
            <div className="mx-8 mt-4 flex flex-none flex-row flex-wrap items-center justify-center gap-4">
                <Button className="h-14 min-w-32 px-10" title={t('RELOAD')} onClick={reload}>
                    {t('RELOAD')}
                </Button>
                <Button
                    variant="ghost"
                    className="h-14 min-w-32 bg-surface-hover px-10 text-danger hover:brightness-110"
                    title={t('CLEAR_DATA')}
                    onClick={() => setConfirmOpen(true)}
                >
                    {t('CLEAR_DATA')}
                </Button>
            </div>

            <Dialog open={confirmOpen} onOpenChange={setConfirmOpen}>
                <DialogContent className="max-w-md gap-6">
                    <DialogHeader>
                        <DialogTitle>{t('CLEAR_DATA')}</DialogTitle>
                        <DialogDescription>
                            This erases the current profile&apos;s library and settings on this device (other
                            profiles are untouched). It cannot be undone.
                        </DialogDescription>
                    </DialogHeader>
                    <DialogFooter>
                        <Button variant="ghost" className="bg-surface-hover text-fg hover:brightness-110" onClick={() => setConfirmOpen(false)}>
                            {t('BUTTON_CANCEL')}
                        </Button>
                        <Button className="bg-danger text-fg hover:brightness-110" onClick={clearData}>
                            {t('CLEAR_DATA')}
                        </Button>
                    </DialogFooter>
                </DialogContent>
            </Dialog>
        </div>
    );
};

export default Error;
