import type {
	AppInfo,
	ChatDownloadTypes,
	ChatItem,
	DownloadProgress,
	IlinkStatus,
	ProxyConfig,
	TelegramStatus
} from '$lib/bindings';
import { commands, events } from '$lib/api';
import { isTauri } from '$lib/runtime';

export const typeOptions = [
	{ key: 'video', label: '视频' },
	{ key: 'audio', label: '音频' },
	{ key: 'photo', label: '图片' },
	{ key: 'document', label: '文档' },
	{ key: 'text', label: '文本' }
] as const;

export function normalizeTypes(types: ChatDownloadTypes): Required<ChatDownloadTypes> {
	return {
		video: !!types.video,
		audio: !!types.audio,
		photo: !!types.photo,
		document: !!types.document,
		text: !!types.text
	};
}

export function hasMediaTypes(types: ChatDownloadTypes): boolean {
	const normalized = normalizeTypes(types);
	return normalized.video || normalized.audio || normalized.photo || normalized.document;
}

function formatError(err: unknown): string {
	if (typeof err === 'string' && err.trim()) return err;
	if (err && typeof err === 'object' && 'message' in err) {
		const message = (err as { message?: unknown }).message;
		if (typeof message === 'string' && message.trim()) return message;
	}
	if (err instanceof Error && err.message.trim()) return err.message;
	return '操作失败，请重试';
}

function unwrap<T>(result: { status: 'ok'; data: T } | { status: 'error'; error: unknown }): T {
	if (result.status === 'ok') {
		return result.data;
	}
	throw new Error(formatError(result.error));
}

class AppState {
	appInfo = $state<AppInfo | null>(null);
	telegram = $state<TelegramStatus | null>(null);
	chats = $state<ChatItem[]>([]);
	selectedChatId = $state<string | null>(null);
	messageEpoch = $state(0);
	error = $state<string | null>(null);
	busy = $state(false);
	download = $state<DownloadProgress | null>(null);
	ilink = $state<IlinkStatus | null>(null);
	phone = $state('');
	code = $state('');
	password = $state('');

	selectedChat = $derived(this.chats.find((chat) => chat.id === this.selectedChatId) ?? null);
	loginStep = $derived(this.telegram?.loginStep ?? 'idle');
	watchedCount = $derived(this.chats.filter((chat) => chat.watched).length);
	authorized = $derived(!!this.telegram?.authorized);
	showMedia = $derived(!!this.telegram?.showMedia);
	syncLabel = $derived.by(() => {
		const progress = this.download;
		if (!progress) return null;
		let label: string | null;
		switch (progress.phase) {
			case 'backfill':
				label = progress.chatTitle ? `回爬「${progress.chatTitle}」` : '回爬历史';
				break;
			case 'live':
				label = progress.chatTitle ? `收新「${progress.chatTitle}」` : '收新消息';
				break;
			case 'floodWait':
				label = `限流等待 ${progress.floodWaitSecs ?? 0}s`;
				break;
			case 'wait':
				label = progress.detail || '等待';
				break;
			case 'reconnect':
				label = progress.detail || '正在重连';
				break;
			default:
				label = progress.detail ?? '空闲';
		}
		if (progress.paused) {
			return label && label !== '空闲' ? `已暂停 · ${label}` : '已暂停';
		}
		return label;
	});

	#started = false;
	#unlisten: Array<() => void> = [];

	async init() {
		if (this.#started) return;
		this.#started = true;

		void events.downloadProgress
			.listen((event) => {
				this.download = event.payload;
			})
			.then((fn) => {
				this.#unlisten.push(fn);
			});

		void events.ilinkStatus
			.listen((event) => {
				this.ilink = event.payload;
			})
			.then((fn) => {
				this.#unlisten.push(fn);
			});

		void events.telegramStatusChanged
			.listen((event) => {
				const { connected, authorized } = event.payload;
				if (this.telegram) {
					this.telegram = {
						...this.telegram,
						connected,
						authorized,
						loginStep: authorized
							? 'authorized'
							: this.telegram.loginStep === 'authorized'
								? 'idle'
								: this.telegram.loginStep
					};
				}
				if (!authorized) {
					this.chats = [];
					this.selectedChatId = null;
					if (this.telegram) this.telegram = { ...this.telegram, account: null };
				}
			})
			.then((fn) => {
				this.#unlisten.push(fn);
			});

		try {
			this.appInfo = await commands.getAppInfo();
		} catch (err) {
			this.error = isTauri()
				? '请通过 pnpm tauri:dev 启动桌面端，才能调用 Rust 命令。'
				: `无法连接 tgd 服务：${formatError(err)}`;
			console.error(err);
			return;
		}

		try {
			await this.applyStatus(await commands.connectTelegram());
			this.download = await commands.getDownloadStatus();
			try {
				this.ilink = await commands.getIlinkStatus();
			} catch {
				// 旧服务没有 iLink 接口时忽略
			}
			if (this.telegram?.authorized) {
				await this.refreshChatsUnlocked();
			}
		} catch (err) {
			this.error = formatError(err);
			console.error(err);
			try {
				const status = await commands.getTelegramStatus();
				if (status.status === 'ok') this.telegram = status.data;
			} catch {
				// 服务已起来，只是 Telegram 还没连上
			}
		}
	}

	destroy() {
		for (const fn of this.#unlisten) fn();
		this.#unlisten = [];
		this.#started = false;
	}

	async logout() {
		await this.withBusy(async () => {
			await this.applyStatus(await commands.logout());
			this.chats = [];
			this.selectedChatId = null;
			this.download = await commands.getDownloadStatus();
			this.phone = '';
			this.code = '';
			this.password = '';
		});
	}

	selectChat(id: string | null) {
		this.selectedChatId = id;
	}

	async applyStatus(
		result: { status: 'ok'; data: TelegramStatus } | { status: 'error'; error: unknown }
	) {
		this.telegram = unwrap(result);
	}

	async refreshStatus() {
		try {
			const status = await commands.getTelegramStatus();
			if (status.status === 'ok') {
				this.telegram = status.data;
			}
		} catch (err) {
			console.error(err);
		}
	}

	async withBusy(action: () => Promise<void>) {
		if (this.busy) return;
		this.busy = true;
		this.error = null;
		try {
			await action();
		} catch (err) {
			this.error = formatError(err);
			await this.refreshStatus();
		} finally {
			this.busy = false;
		}
	}

	async sendCode() {
		await this.withBusy(async () => {
			await this.applyStatus(await commands.requestLoginCode(this.phone.trim()));
		});
	}

	async confirmCode() {
		await this.withBusy(async () => {
			await this.applyStatus(await commands.submitLoginCode(this.code.trim()));
			if (this.telegram?.authorized) {
				this.code = '';
				this.password = '';
				await this.refreshChatsUnlocked();
			}
		});
	}

	async confirmPassword() {
		await this.withBusy(async () => {
			await this.applyStatus(await commands.submitPassword(this.password));
			if (this.telegram?.authorized) {
				this.code = '';
				this.password = '';
				await this.refreshChatsUnlocked();
			}
		});
	}

	async refreshChatsUnlocked(refresh = false) {
		this.chats = unwrap(await commands.listChats(refresh));
		if (this.selectedChatId && !this.chats.some((chat) => chat.id === this.selectedChatId)) {
			this.selectedChatId = null;
		}
	}

	async refreshChats() {
		await this.withBusy(() => this.refreshChatsUnlocked(true));
	}

	async changeDownloadDir() {
		await this.withBusy(async () => {
			await this.applyStatus(await commands.pickDownloadDir());
		});
	}

	async setDownloadDir(dir: string) {
		await this.withBusy(async () => {
			await this.applyStatus(await commands.setDownloadDir(dir));
		});
	}

	async setGuestWatch(enabled: boolean, query: string) {
		await this.withBusy(async () => {
			await this.applyStatus(await commands.setGuestWatch(enabled, query));
			if (this.telegram?.authorized) {
				await this.refreshChatsUnlocked();
			}
		});
	}

	async addGuestWatch(query: string) {
		await this.withBusy(async () => {
			await this.applyStatus(await commands.addGuestWatch(query));
			if (this.telegram?.authorized) {
				await this.refreshChatsUnlocked();
			}
		});
	}

	async removeGuestWatch(chatId: string) {
		await this.withBusy(async () => {
			await this.applyStatus(await commands.removeGuestWatch(chatId));
			if (this.telegram?.authorized) {
				await this.refreshChatsUnlocked();
			}
		});
	}

	async setGuestWatchEnabled(chatId: string, enabled: boolean) {
		await this.withBusy(async () => {
			await this.applyStatus(await commands.setGuestWatchEnabled(chatId, enabled));
			if (this.telegram?.authorized) {
				await this.refreshChatsUnlocked();
			}
		});
	}

	async setWatched(chat: ChatItem, watched: boolean) {
		if (chat.watched === watched) return;
		const previous = chat.watched;
		this.chats = this.chats.map((item) => (item.id === chat.id ? { ...item, watched } : item));
		try {
			unwrap(await commands.setChatWatched(chat.id, watched));
			await this.refreshChatsUnlocked();
		} catch (err) {
			this.chats = this.chats.map((item) =>
				item.id === chat.id ? { ...item, watched: previous } : item
			);
			this.error = formatError(err);
		}
	}

	async setType(chat: ChatItem, key: keyof Required<ChatDownloadTypes>, value: boolean) {
		const previous = normalizeTypes(chat.types);
		if (previous[key] === value) return;
		const next = { ...previous, [key]: value };
		this.chats = this.chats.map((item) => (item.id === chat.id ? { ...item, types: next } : item));
		try {
			const saved = unwrap(await commands.setChatDownloadTypes(chat.id, next));
			const types = normalizeTypes(saved);
			this.chats = this.chats.map((item) => {
				if (item.id === chat.id) return { ...item, types };
				if (chat.kind === 'channel' && chat.discussionId && item.id === chat.discussionId) {
					return { ...item, types };
				}
				return item;
			});
		} catch (err) {
			this.chats = this.chats.map((item) =>
				item.id === chat.id ? { ...item, types: previous } : item
			);
			this.error = formatError(err);
		}
	}

	async setAutostart(enabled: boolean) {
		if (this.telegram?.autostart === enabled) return;
		const previous = this.telegram?.autostart ?? false;
		if (this.telegram) this.telegram = { ...this.telegram, autostart: enabled };
		try {
			const saved = unwrap(await commands.setAutostart(enabled));
			if (this.telegram) this.telegram = { ...this.telegram, autostart: saved };
		} catch (err) {
			if (this.telegram) this.telegram = { ...this.telegram, autostart: previous };
			this.error = formatError(err);
		}
	}

	async setProxy(config: ProxyConfig) {
		await this.withBusy(async () => {
			await this.applyStatus(await commands.setProxy(config));
		});
	}

	async setShowMedia(enabled: boolean) {
		if (this.telegram?.showMedia === enabled) return;
		const previous = this.telegram?.showMedia ?? false;
		if (this.telegram) this.telegram = { ...this.telegram, showMedia: enabled };
		try {
			const saved = unwrap(await commands.setShowMedia(enabled));
			if (this.telegram) this.telegram = { ...this.telegram, showMedia: saved };
		} catch (err) {
			if (this.telegram) this.telegram = { ...this.telegram, showMedia: previous };
			this.error = formatError(err);
		}
	}

	async setGlobalBackfill(raw: string) {
		const days = Number(raw);
		if (!Number.isFinite(days)) return;
		const value = Math.floor(days);
		try {
			const saved = unwrap(await commands.setBackfillDays(value));
			if (this.telegram) this.telegram = { ...this.telegram, backfillDays: saved };
			this.chats = this.chats.map((item) =>
				item.backfillDaysOverride == null ? { ...item, backfillDays: saved } : item
			);
		} catch (err) {
			this.error = formatError(err);
		}
	}

	async setDownloadConcurrency(raw: string) {
		const n = Number(raw);
		if (!Number.isFinite(n)) return;
		const value = Math.floor(n);
		try {
			const saved = unwrap(await commands.setDownloadConcurrency(value));
			if (this.telegram) this.telegram = { ...this.telegram, downloadConcurrency: saved };
		} catch (err) {
			this.error = formatError(err);
		}
	}

	async setMinMediaMb(raw: string) {
		const n = Number(raw);
		if (!Number.isFinite(n) || n < 0) return;
		try {
			const saved = unwrap(await commands.setMinMediaMb(n));
			if (this.telegram) this.telegram = { ...this.telegram, minMediaMb: saved };
		} catch (err) {
			this.error = formatError(err);
		}
	}

	async refreshIlink() {
		try {
			this.ilink = await commands.getIlinkStatus();
		} catch (err) {
			this.error = formatError(err);
		}
	}

	async setIlinkEnabled(enabled: boolean) {
		const previous = this.ilink;
		if (this.ilink) this.ilink = { ...this.ilink, enabled };
		try {
			this.ilink = unwrap(await commands.setIlinkNotifyEnabled(enabled));
		} catch (err) {
			this.ilink = previous;
			this.error = formatError(err);
		}
	}

	async startIlinkLogin() {
		try {
			this.ilink = unwrap(await commands.ilinkStartLogin());
		} catch (err) {
			this.error = formatError(err);
		}
	}

	async logoutIlink() {
		try {
			this.ilink = unwrap(await commands.ilinkLogout());
		} catch (err) {
			this.error = formatError(err);
		}
	}

	async sendIlinkTest() {
		try {
			this.ilink = unwrap(await commands.ilinkSendTest());
		} catch (err) {
			this.error = formatError(err);
		}
	}

	async setDownloadPaused(paused: boolean) {
		if (this.download?.paused === paused) return;
		const previous = this.download?.paused ?? false;
		if (this.download) this.download = { ...this.download, paused };
		try {
			const saved = unwrap(await commands.setDownloadPaused(paused));
			if (this.download) this.download = { ...this.download, paused: saved };
		} catch (err) {
			if (this.download) this.download = { ...this.download, paused: previous };
			this.error = formatError(err);
		}
	}

	async cancelDownload(fileId: string) {
		if (this.download) {
			this.download = {
				...this.download,
				active: this.download.active.filter((item) => item.fileId !== fileId),
				queued: this.download.queued.filter((item) => item.fileId !== fileId)
			};
		}
		try {
			unwrap(await commands.cancelDownload(fileId));
		} catch (err) {
			this.error = formatError(err);
		}
	}

	async openPath(path: string) {
		try {
			unwrap(await commands.openPath(path));
		} catch (err) {
			this.error = formatError(err);
		}
	}

	async openDownloadDir() {
		try {
			unwrap(await commands.openDownloadDir());
		} catch (err) {
			this.error = formatError(err);
		}
	}

	async setChatAlias(chat: ChatItem, raw: string) {
		const next = raw.trim() === '' ? null : raw.trim();
		const previous = chat.alias;
		if ((previous ?? null) === next) return;
		this.chats = this.chats.map((item) => (item.id === chat.id ? { ...item, alias: next } : item));
		try {
			const saved = unwrap(await commands.setChatAlias(chat.id, next));
			this.chats = this.chats.map((item) =>
				item.id === chat.id ? { ...item, alias: saved } : item
			);
		} catch (err) {
			this.chats = this.chats.map((item) =>
				item.id === chat.id ? { ...item, alias: previous } : item
			);
			this.error = formatError(err);
		}
	}

	async setChatBackfill(chat: ChatItem, raw: string) {
		const trimmed = raw.trim();
		const days = trimmed === '' ? null : Number(trimmed);
		if (days != null && !Number.isFinite(days)) return;
		const next = days == null ? null : Math.floor(days);
		const previous = chat.backfillDaysOverride;
		this.chats = this.chats.map((item) =>
			item.id === chat.id ? { ...item, backfillDaysOverride: next } : item
		);
		try {
			const effective = unwrap(await commands.setChatBackfillDays(chat.id, next));
			this.chats = this.chats.map((item) => {
				if (item.id === chat.id) {
					return { ...item, backfillDays: effective, backfillDaysOverride: next };
				}
				if (chat.kind === 'channel' && chat.discussionId && item.id === chat.discussionId) {
					return { ...item, backfillDays: effective, backfillDaysOverride: next };
				}
				return item;
			});
		} catch (err) {
			this.chats = this.chats.map((item) =>
				item.id === chat.id ? { ...item, backfillDaysOverride: previous } : item
			);
			this.error = formatError(err);
		}
	}

	async clearChatMessages(chat: ChatItem) {
		await this.withBusy(async () => {
			unwrap(await commands.clearChatMessages(chat.id));
			this.messageEpoch += 1;
		});
	}

	async checkChatMedia(chat: ChatItem) {
		try {
			unwrap(await commands.checkChatMedia(chat.id));
		} catch (err) {
			this.error = formatError(err);
		}
	}

	onPasswordKeydown(event: KeyboardEvent) {
		if (event.key === 'Enter' && !this.busy && this.password.trim()) {
			event.preventDefault();
			void this.confirmPassword();
		}
	}
}

export const app = new AppState();
