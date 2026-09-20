import React, { forwardRef, useCallback, useEffect, useMemo, useState } from 'react';
import { useTranslation } from 'react-i18next';
import { useCore } from 'rillio/core';
import { usePlatform, useToast, useDiscord } from 'rillio/common';
import { Button } from 'rillio/components/ui/button';
import { Trakt, Discord } from 'rillio/components/ui/brand-icons';
import { Section, Option, Link, SettingsSwitch } from '../components';
import User from './User';
import useDataExport from './useDataExport';
import useUpdateCheck from './useUpdateCheck';

// CJS require, matching how the rest of the app consumes usePlayUrl (it is an
// `export =` module).
const { default: usePlayUrl } = require('rillio/common/usePlayUrl');

type Props = {
    profile: Profile,
};

const General = forwardRef<HTMLDivElement, Props>(({ profile }: Props, ref) => {
    const { t } = useTranslation();
    const core = useCore();
    const platform = usePlatform();
    const toast = useToast();
    const discord = useDiscord();
    const [dataExport, loadDataExport] = useDataExport();
    const { handlePlayUrl } = usePlayUrl();
    const update = useUpdateCheck();
    // The SHELL's version, not process.env.VERSION: that one is the web bundle's
    // (still upstream's 5.0.0-beta.x) and showing it next to an updater that
    // ships Rillio 0.1.x reads as a different app entirely.
    const appVersion = typeof platform.shell.state.version === 'string' ?
        platform.shell.state.version
        :
        process.env.VERSION;

    const [traktAuthStarted, setTraktAuthStarted] = useState(false);

    const isTraktAuthenticated = useMemo(() => {
        const trakt = profile?.auth?.user?.trakt;
        return trakt && (Date.now() / 1000) < (trakt.created_at + trakt.expires_in);
    }, [profile.auth]);

    const onExportData = useCallback(() => {
        loadDataExport();
    }, []);

    // Moved here from the account menu, which is the account and nothing else now.
    const onPlayUrl = useCallback(async () => {
        try {
            const clipboardText = await navigator.clipboard.readText();
            const handled = await handlePlayUrl(clipboardText);
            if (!handled) {
                toast.show({
                    type: 'error',
                    title: 'Clipboard does not contain a valid URL or magnet link.',
                    timeout: 5000
                });
            }
        } catch (e) {
            console.error(e);
        }
    }, [handlePlayUrl, toast]);

    const onCalendarSubscribe = useCallback(() => {
        if (!profile.auth) return;

        const protocol = platform.name === 'ios' ? 'webcal' : 'https';
        const url = `${protocol}://www.strem.io/calendar/${profile.auth.user._id}.ics`;
        platform.openExternal(url);

        toast.show({
            type: 'success',
            title: platform.name === 'ios' ?
                t('SETTINGS_SUBSCRIBE_CALENDAR_IOS_TOAST') :
                t('SETTINGS_SUBSCRIBE_CALENDAR_TOAST'),
            timeout: 25000
        });
        // Stremio 4 emits not documented event subscribeCalendar
    }, [profile.auth]);

    const onToggleTrakt = useCallback(() => {
        if (!isTraktAuthenticated && profile.auth !== null && profile.auth.user !== null && typeof profile.auth.user._id === 'string') {
            platform.openExternal(`https://www.strem.io/trakt/auth/${profile.auth.user._id}`);
            setTraktAuthStarted(true);
        } else {
            core.transport.dispatch({
                action: 'Ctx',
                args: {
                    action: 'LogoutTrakt'
                }
            });
        }
    }, [isTraktAuthenticated, profile.auth]);

    const discordToggle = useMemo(() => ({
        checked: profile.settings.discordRpcEnabled === true,
        onClick: () => {
            core.transport.dispatch({
                action: 'Ctx',
                args: {
                    action: 'UpdateSettings',
                    args: {
                        ...profile.settings,
                        discordRpcEnabled: !profile.settings.discordRpcEnabled
                    }
                }
            });
        }
    }), [profile.settings]);

    useEffect(() => {
        if (dataExport.exportUrl) {
            platform.openExternal(dataExport.exportUrl);
        }
    }, [dataExport.exportUrl]);

    useEffect(() => {
        if (isTraktAuthenticated && traktAuthStarted) {
            core.transport.dispatch({
                action: 'Ctx',
                args: {
                    action: 'InstallTraktAddon'
                }
            });
            setTraktAuthStarted(false);
        }
    }, [isTraktAuthenticated, traktAuthStarted]);

    return <>
        <Section ref={ref}>
            <User profile={profile} />
        </Section>

        {
            // Right under the account block, because until now the launch-time
            // toast was the ONLY way to take an update: miss it and there was
            // no in-app path at all. Desktop shell only (browser and Android
            // have no in-app updater).
            update.available ?
                <Section>
                    <Option label={'Updates'}>
                        <div className="flex w-full items-center gap-4">
                            <Button
                                variant="ghost"
                                onClick={update.status.phase === 'available' ? update.install : update.check}
                                disabled={update.status.phase === 'checking' || update.status.phase === 'installing'}
                                tabIndex={-1}
                                className="h-14 shrink-0 bg-surface-hover px-8 font-medium text-fg hover:brightness-110 active:scale-[0.97] disabled:opacity-60"
                            >
                                {
                                    update.status.phase === 'checking' ? 'Checking...' :
                                        update.status.phase === 'installing' ? 'Installing...' :
                                            update.status.phase === 'available' ? 'Install and restart' :
                                                'Check for updates'
                                }
                            </Button>
                            <div className="min-w-0 text-sm text-fg-muted">
                                {
                                    update.status.phase === 'available' ? `Rillio ${update.status.version} is available` :
                                        update.status.phase === 'up-to-date' ? `Rillio ${appVersion} is up to date` :
                                            update.status.phase === 'failed' ? <span className="text-danger">Could not check for updates</span> :
                                                `Version ${appVersion}`
                                }
                            </div>
                        </div>
                    </Option>
                </Section>
                :
                null
        }

        <Section>
            {/* Play URL/Magnet came from the account menu, which is the account and
                nothing else now. Sync & backup came with it and then went straight back
                out: the User block above (Settings > General's own first section) has
                carried that link all along, so adding one here just built the same
                duplication this pass exists to remove. */}
            <Link
                label={t('PLAY_URL_MAGNET_LINK')}
                onClick={onPlayUrl}
            />
            {
                profile?.auth?.user &&
                    <Link
                        label={t('SETTINGS_DATA_EXPORT')}
                        onClick={onExportData}
                    />
            }
            {
                profile?.auth?.user &&
                    <Link
                        label={t('SETTINGS_SUBSCRIBE_CALENDAR')}
                        onClick={onCalendarSubscribe}
                    />
            }
            <Link
                label={t('SETTINGS_SUPPORT')}
                href={'https://github.com/RillioApp/rillio/issues'}
            />
            <Link
                label={t('SETTINGS_SOURCE_CODE')}
                href={'https://github.com/RillioApp/rillio'}
            />
            <Link
                label={'Website'}
                href={'https://rillio.app'}
            />
            {
                profile?.auth?.user?.email &&
                    <Link
                        label={t('SETTINGS_CHANGE_PASSWORD')}
                        href={`https://www.strem.io/reset-password/${profile.auth.user.email}`}
                    />
            }
            <Option className="mt-8" icon={Trakt} iconClassName="text-[color:var(--color-trakt)]" label={t('SETTINGS_TRAKT')}>
                <Button
                    variant="ghost"
                    title={isTraktAuthenticated ? t('LOG_OUT') : t('SETTINGS_TRAKT_AUTHENTICATE')}
                    disabled={profile.auth === null}
                    tabIndex={-1}
                    onClick={onToggleTrakt}
                    className="h-14 w-full bg-surface-hover px-8 font-medium text-fg hover:brightness-110 active:scale-[0.97]"
                >
                    {isTraktAuthenticated ? t('LOG_OUT') : t('SETTINGS_TRAKT_AUTHENTICATE')}
                </Button>
            </Option>
            {
                discord.available &&
                    <Option icon={Discord} iconClassName="text-[color:var(--color-discord)]" label={'SETTINGS_DISCORD'}>
                        <SettingsSwitch
                            {...discordToggle}
                        />
                    </Option>
            }
        </Section>
    </>;
});

General.displayName = 'General';

export default General;
