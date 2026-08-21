import { commands as desktopCommands, events as desktopEvents } from '$lib/bindings';
import { httpCommands, httpEvents } from '$lib/api-http';
import { isTauri } from '$lib/runtime';

export const commands = isTauri() ? desktopCommands : httpCommands;
export const events = isTauri() ? desktopEvents : httpEvents;
