<script lang="ts">
	import {
		Globe,
		HardDriveDownload,
		Inbox,
		LogOut,
		MessageCircle,
		Network,
		Plus,
		ScrollText,
		Trash2
	} from '@lucide/svelte';

	import { app } from '$lib/app-state.svelte';
	import type { DownloadUsage, LogEntry } from '$lib/bindings';
	import { commands } from '$lib/api';
	import { httpCommands } from '$lib/api-http';
	import { isTauri } from '$lib/runtime';
	import { Badge } from '$lib/components/ui/badge';
	import { Button } from '$lib/components/ui/button';
	import * as Card from '$lib/components/ui/card';
	import { Input } from '$lib/components/ui/input';
	import { Label } from '$lib/components/ui/label';
	import { Switch } from '$lib/components/ui/switch';

	let usage = $state<DownloadUsage | null>(null);
	let proxyEnabled = $state(false);
	let proxyHost = $state('');
	let proxyPort = $state('1080');
	let proxyUser = $state('');
	let proxyPassword = $state('');
	let proxyHydrated = $state(false);
	let downloadDirDraft = $state('');
	let downloadDirOptions = $state<string[]>(['/downloads']);
	let newGuestQuery = $state('');
	let logs = $state<LogEntry[]>([]);
	let logsBusy = $state(false);

	$effect(() => {
		const proxy = app.telegram?.proxy;
		if (!proxy || proxyHydrated) return;
		proxyEnabled = proxy.enabled;
		proxyHost = proxy.host ?? '';
		proxyPort = proxy.port ? String(proxy.port) : '1080';
		proxyUser = proxy.username ?? '';
		proxyHydrated = true;
	});

	async function addGuestWatch() {
		const query = newGuestQuery.trim();
		if (!query) {
			app.error = '请填写公开用户名或 t.me 链接';
			return;
		}
		await app.addGuestWatch(query);
		newGuestQuery = '';
	}

	async function saveProxy() {
		const port = Number(proxyPort);
		if (proxyEnabled && (!proxyHost.trim() || !Number.isInteger(port) || port < 1 || port > 65535)) {
			app.error = '请填写有效的 SOCKS5 主机和端口';
			return;
		}
		await app.setProxy({
			enabled: proxyEnabled,
			host: proxyHost.trim(),
			port: proxyEnabled ? port : 0,
			username: proxyUser.trim() || null,
			password: proxyPassword.trim() ? proxyPassword : null,
			hasPassword: !!app.telegram?.proxy?.hasPassword
		});
		proxyPassword = '';
		proxyHydrated = false;
	}

	function formatSize(raw: string | null | undefined): string {
		const n = Number(raw);
		if (!Number.isFinite(n) || n < 0) return '';
		if (n < 1024) return `${Math.round(n)} B`;
		if (n < 1024 * 1024) return `${(n / 1024).toFixed(1)} KB`;
		if (n < 1024 * 1024 * 1024) return `${(n / 1024 / 1024).toFixed(1)} MB`;
		return `${(n / 1024 / 1024 / 1024).toFixed(2)} GB`;
	}

	function kindUsageLine(kinds: DownloadUsage['kinds']): string {
		return (
			[
				['图片', kinds.photo],
				['视频', kinds.video],
				['音频', kinds.audio],
				['文档', kinds.document],
				['其他', kinds.other]
			] as const
		)
			.filter(([, n]) => Number(n) > 0)
			.map(([label, n]) => `${label} ${formatSize(n)}`)
			.join(' · ');
	}

	function usageChatLabel(chat: DownloadUsage['chats'][number]): string {
		return chat.alias?.trim() || chat.chatTitle?.trim() || chat.chatId || '未知会话';
	}

	$effect(() => {
		void app.telegram?.downloadDir;
		void commands.getDownloadUsage().then((result) => {
			if (result.status === 'ok') usage = result.data;
		});
	});

	$effect(() => {
		const dir = app.telegram?.downloadDir;
		if (dir) downloadDirDraft = dir;
	});

	$effect(() => {
		if (isTauri()) return;
		void httpCommands.getDownloadDirOptions().then((opts) => {
			if (opts.available?.length) downloadDirOptions = opts.available;
		});
	});

	async function loadLogs(manual = false) {
		if (manual) logsBusy = true;
		try {
			logs = await commands.getRecentLogs();
		} catch {
			// 拉日志失败不挡设置页
		} finally {
			if (manual) logsBusy = false;
		}
	}

	$effect(() => {
		void loadLogs();
		const timer = window.setInterval(() => void loadLogs(), 5000);
		return () => window.clearInterval(timer);
	});

	$effect(() => {
		const state = app.ilink?.qrState;
		if (state !== 'wait' && state !== 'scanned') return;
		const timer = window.setInterval(() => void app.refreshIlink(), 1500);
		return () => window.clearInterval(timer);
	});

	function qrLabel(state: string | undefined): string {
		switch (state) {
			case 'wait':
				return '等待扫码';
			case 'scanned':
				return '已扫码，请在手机上确认';
			case 'expired':
				return '二维码已过期';
			case 'confirmed':
				return '已登录';
			default:
				return '';
		}
	}
</script>

<div class="h-full overflow-y-auto">
	<div class="mx-auto flex max-w-2xl flex-col gap-4 px-3 py-4 md:px-6 md:py-6">
		<Card.Root class="rounded-3xl border border-border/50 bg-card shadow-xs">
			<Card.Header>
				<Card.Title class="flex items-center gap-2.5">
					<div class="flex size-8 items-center justify-center rounded-2xl bg-primary/10 text-primary">
						<Inbox class="size-4" />
					</div>
					<span>应用</span>
				</Card.Title>
				<Card.Description>bundle 与版本来自 Rust / tauri-specta</Card.Description>
			</Card.Header>
			<Card.Content class="space-y-3 text-sm">
				<p>名称：{app.appInfo?.name ?? '…'}</p>
				<p>版本：{app.appInfo?.version ?? '…'}</p>
				<p>标识：{app.appInfo?.identifier ?? '…'}</p>
				{#if isTauri()}
					<div class="flex items-center justify-between gap-3 pt-1">
						<div class="min-w-0">
							<Label for="autostart" class="text-sm font-normal">开机自启</Label>
							<p class="text-xs text-muted-foreground">登录系统后在托盘启动，不弹出窗口</p>
						</div>
						<Switch
							id="autostart"
							checked={!!app.telegram?.autostart}
							onCheckedChange={(value) => void app.setAutostart(value)}
							disabled={app.busy}
						/>
					</div>
				{/if}
			</Card.Content>
		</Card.Root>

		<Card.Root class="rounded-3xl border border-border/50 bg-card shadow-xs">
			<Card.Header>
				<Card.Title class="flex items-center gap-2.5">
					<div class="flex size-8 items-center justify-center rounded-2xl bg-sky-500/10 text-sky-600 dark:text-sky-400">
						<HardDriveDownload class="size-4" />
					</div>
					<span>Telegram</span>
				</Card.Title>
				<Card.Description>
					{isTauri()
						? '凭据来自仓库根目录 .env，会话写在 app data'
						: '凭据来自容器环境变量，会话写在数据卷'}
				</Card.Description>
			</Card.Header>
			<Card.Content class="space-y-3 text-sm">
				<div class="flex items-center gap-2">
					<span>连接</span>
					<Badge variant={app.telegram?.connected ? 'default' : 'secondary'}>
						{app.telegram?.connected ? '已连接' : '未连接'}
					</Badge>
				</div>
				<div class="flex items-center gap-2">
					<span>授权</span>
					<Badge variant={app.telegram?.authorized ? 'default' : 'secondary'}>
						{app.telegram?.authorized ? '已登录' : '未登录'}
					</Badge>
				</div>
				<div class="flex items-center gap-2">
					<span>会话文件</span>
					<Badge variant={app.telegram?.sessionExists ? 'default' : 'outline'}>
						{app.telegram?.sessionExists ? '已存在' : '无'}
					</Badge>
				</div>
				<div class="space-y-3 border-t pt-3">
					<div class="flex items-center justify-between gap-3">
						<div class="min-w-0">
							<Label for="proxy-enabled" class="flex items-center gap-1.5 text-sm font-normal">
								<Network class="size-3.5" />
								SOCKS5 代理
							</Label>
							<p class="text-xs text-muted-foreground">
								仅 SOCKS5（Clash 一般是 7891）。飞牛容器请填代理机局域网 IP，不要填 127.0.0.1。
							</p>
						</div>
						<Switch
							id="proxy-enabled"
							checked={proxyEnabled}
							onCheckedChange={(value) => (proxyEnabled = value)}
							disabled={app.busy}
						/>
					</div>
					{#if proxyEnabled}
						<div class="grid grid-cols-1 gap-2 sm:grid-cols-3">
							<div class="space-y-1 sm:col-span-2">
								<Label for="proxy-host" class="text-xs font-normal text-muted-foreground">主机</Label>
								<Input
									id="proxy-host"
									class="h-8"
									placeholder="192.168.5.2"
									bind:value={proxyHost}
									disabled={app.busy}
								/>
							</div>
							<div class="space-y-1">
								<Label for="proxy-port" class="text-xs font-normal text-muted-foreground">端口</Label>
								<Input
									id="proxy-port"
									class="h-8"
									inputmode="numeric"
									placeholder="7891"
									bind:value={proxyPort}
									disabled={app.busy}
								/>
							</div>
						</div>
						<div class="grid grid-cols-1 gap-2 sm:grid-cols-2">
							<div class="space-y-1">
								<Label for="proxy-user" class="text-xs font-normal text-muted-foreground">用户名</Label>
								<Input
									id="proxy-user"
									class="h-8"
									placeholder="可选"
									bind:value={proxyUser}
									disabled={app.busy}
									autocomplete="off"
								/>
							</div>
							<div class="space-y-1">
								<Label for="proxy-password" class="text-xs font-normal text-muted-foreground">
									密码{app.telegram?.proxy?.hasPassword ? '（已保存，留空则保留）' : ''}
								</Label>
								<Input
									id="proxy-password"
									class="h-8"
									type="password"
									placeholder={app.telegram?.proxy?.hasPassword ? '不变' : '可选'}
									bind:value={proxyPassword}
									disabled={app.busy}
									autocomplete="new-password"
								/>
							</div>
						</div>
					{/if}
					<div class="flex justify-end">
						<Button size="sm" class="rounded-full px-4 text-xs font-medium" onclick={() => void saveProxy()} disabled={app.busy}>
							{app.busy ? '连接中…' : '保存并重连'}
						</Button>
					</div>
				</div>
				{#if app.authorized}
					<div class="space-y-3 border-t pt-3">
						<div class="flex items-center justify-between gap-3">
							<div class="min-w-0">
								<Label class="flex items-center gap-1.5 text-sm font-normal">
									<Globe class="size-3.5" />
									未加入公开频道
								</Label>
								<p class="text-xs text-muted-foreground">
									不加入，直接监听并预览公开群/频道。与已加入的监听并存。实时约 45
									秒拉一次；下载类型和天数可在会话列表设置。
								</p>
							</div>
						</div>
						<form
							class="flex flex-col gap-2 sm:flex-row"
							onsubmit={(e) => {
								e.preventDefault();
								void addGuestWatch();
							}}
						>
							<Input
								id="guest-query"
								class="min-w-0 flex-1 rounded-full px-3 text-xs"
								placeholder="https://t.me/xxx 或 @xxx"
								bind:value={newGuestQuery}
								disabled={app.busy}
							/>
							<Button
								type="submit"
								variant="outline"
								size="sm"
								class="shrink-0 rounded-full px-4 text-xs"
								disabled={app.busy || !newGuestQuery.trim()}
							>
								<Plus class="mr-1 size-3.5" />
								添加
							</Button>
						</form>
						{#if app.telegram?.guestWatches && app.telegram.guestWatches.length > 0}
							<div class="divide-y divide-border/40 rounded-2xl border border-border/50 text-sm overflow-hidden bg-muted/20">
								{#each app.telegram.guestWatches as item (item.chatId)}
									<div class="flex items-center justify-between gap-3 p-2.5">
										<div class="min-w-0 flex-1">
											<div class="flex items-center gap-1.5 truncate">
												<span class="font-medium truncate">{item.title || item.chatId}</span>
												{#if item.username}
													<span class="text-xs text-muted-foreground truncate">@{item.username}</span>
												{/if}
												<Badge variant="outline" class="text-[10px] px-1.5 py-0 shrink-0 rounded-full">
													{item.kind === 'group' ? '群组' : '频道'}
												</Badge>
											</div>
											<p class="text-xs text-muted-foreground truncate">{item.query || item.chatId}</p>
										</div>
										<div class="flex items-center gap-2 shrink-0">
											<Switch
												checked={item.enabled}
												onCheckedChange={(val) => void app.setGuestWatchEnabled(item.chatId, val)}
												disabled={app.busy}
												aria-label="开关监听"
											/>
											<Button
												variant="ghost"
												size="icon-sm"
												class="text-muted-foreground hover:text-destructive size-7 rounded-full"
												onclick={() => void app.removeGuestWatch(item.chatId)}
												disabled={app.busy}
												title="删除"
												aria-label="删除"
											>
												<Trash2 class="size-3.5" />
											</Button>
										</div>
									</div>
								{/each}
							</div>
						{:else}
							<p class="text-xs text-muted-foreground">暂未添加未加入公开频道。</p>
						{/if}
					</div>
					<div class="space-y-1 pt-1">
						<p class="text-sm font-medium">当前账号</p>
						{#if app.telegram?.account}
							<p>{app.telegram.account.name}</p>
							<p class="text-xs text-muted-foreground">
								{app.telegram.account.username ? `@${app.telegram.account.username}` : '无用户名'}
								{#if app.telegram.account.phone}
									· {app.telegram.account.phone}
								{/if}
							</p>
						{:else}
							<p class="text-xs text-muted-foreground">已登录，身份尚未拉到</p>
						{/if}
					</div>
					<div class="flex items-center justify-between gap-3 pt-1">
						<div class="min-w-0">
							<p class="text-sm">退出登录</p>
							<p class="text-xs text-muted-foreground">
								作废 Telegram 会话，本地消息和已下载文件保留
							</p>
						</div>
						<Button
							variant="outline"
							size="sm"
							class="shrink-0 rounded-full px-4 text-xs"
							onclick={() => {
								if (confirm('退出登录？需要重新验证码。本地消息和已下载文件会保留。')) {
									void app.logout();
								}
							}}
							disabled={app.busy}
						>
							退出登录
						</Button>
					</div>
				{/if}
				<div class="space-y-2">
					<p class="min-w-0 break-all text-muted-foreground">
						下载目录：{app.telegram?.downloadDir ?? '…'}
					</p>
					{#if isTauri()}
						<div class="flex gap-2">
							<Button
								variant="outline"
								size="sm"
								class="rounded-full px-4 text-xs"
								onclick={() => void app.openDownloadDir()}
								disabled={app.busy}
							>
								打开
							</Button>
							<Button
								variant="outline"
								size="sm"
								class="rounded-full px-4 text-xs"
								onclick={() => void app.changeDownloadDir()}
								disabled={app.busy}
							>
								更改…
							</Button>
						</div>
					{:else}
						<div class="flex flex-col gap-2 sm:flex-row">
							<Input
								class="min-w-0 flex-1 rounded-full px-3 text-xs"
								list="tgd-download-dirs"
								bind:value={downloadDirDraft}
								placeholder="/downloads 或 /vol1/…"
								disabled={app.busy}
							/>
							<datalist id="tgd-download-dirs">
								{#each downloadDirOptions as option}
									<option value={option}></option>
								{/each}
							</datalist>
							<Button
								variant="outline"
								size="sm"
								class="shrink-0 rounded-full px-4 text-xs"
								onclick={() => void app.setDownloadDir(downloadDirDraft)}
								disabled={app.busy || !downloadDirDraft.trim()}
							>
								保存
							</Button>
						</div>
						<p class="text-xs text-muted-foreground">
							飞牛原生包可直接访问 /vol*。「wj 的文件/tgd」一般是
							<code>/vol1/数字/tgd</code>。Compose 自用需把目录挂进容器。
						</p>
					{/if}
				</div>
				{#if usage}
					<div class="text-muted-foreground space-y-0.5 text-xs">
						<p>
							占用 {formatSize(usage.total) || '0 B'}
							{#if Number(usage.parts) > 0}
								（未完成 {formatSize(usage.parts)}）
							{/if}
						</p>
						{#if kindUsageLine(usage.kinds)}
							<p>{kindUsageLine(usage.kinds)}</p>
						{/if}
						{#if usage.chats.length > 0}
							<p>
								{usage.chats
									.slice(0, 4)
									.map((chat) => `${usageChatLabel(chat)} ${formatSize(chat.bytes)}`)
									.join(' · ')}
								{#if usage.chats.length > 4}
									· 共 {usage.chats.length} 个会话
								{/if}
							</p>
						{/if}
					</div>
				{/if}
				<div class="flex items-center justify-between gap-3">
					<Label for="backfill-days" class="text-xs font-normal text-muted-foreground">
						全局回爬天数（0 = 只收新，&lt;0 = 全量）
					</Label>
					<Input
						id="backfill-days"
						type="number"
						step="1"
						class="h-8 w-20"
						value={app.telegram?.backfillDays ?? 0}
						disabled={app.busy}
						onchange={(event) => void app.setGlobalBackfill(event.currentTarget.value)}
					/>
				</div>
				<div class="flex items-center justify-between gap-3">
					<div class="min-w-0">
						<Label for="download-concurrency" class="text-xs font-normal text-muted-foreground">
							回爬并行下载数（1–8）
						</Label>
						<p class="text-[11px] text-muted-foreground">只影响历史回爬，实时新消息仍串行</p>
					</div>
					<Input
						id="download-concurrency"
						type="number"
						min="1"
						max="8"
						step="1"
						class="h-8 w-20"
						value={app.telegram?.downloadConcurrency ?? 2}
						disabled={app.busy}
						onchange={(event) => void app.setDownloadConcurrency(event.currentTarget.value)}
					/>
				</div>
				<div class="flex items-center justify-between gap-3">
					<div class="min-w-0">
						<Label for="min-media-mb" class="text-xs font-normal text-muted-foreground">
							跳过小于（MB）
						</Label>
						<p class="text-[11px] text-muted-foreground">
							0 = 不过滤。只拦下载，文本仍入库；照片有时没有体积，仍会下载
						</p>
					</div>
					<Input
						id="min-media-mb"
						type="number"
						min="0"
						max="4096"
						step="0.1"
						class="h-8 w-20"
						value={app.telegram?.minMediaMb ?? 0}
						disabled={app.busy}
						onchange={(event) => void app.setMinMediaMb(event.currentTarget.value)}
					/>
				</div>
				<div class="flex items-center justify-between gap-3">
					<div class="min-w-0">
						<Label for="show-media" class="text-sm font-normal">消息列表显示媒体</Label>
						<p class="text-xs text-muted-foreground">仅展示已下载的图片 / 视频 / 音频</p>
					</div>
					<Switch
						id="show-media"
						checked={app.showMedia}
						onCheckedChange={(value) => void app.setShowMedia(value)}
						disabled={app.busy}
					/>
				</div>
			</Card.Content>
		</Card.Root>

		<Card.Root class="rounded-3xl border border-border/50 bg-card shadow-xs">
			<Card.Header>
				<Card.Title class="flex items-center gap-2.5">
					<div class="flex size-8 items-center justify-center rounded-2xl bg-emerald-500/10 text-emerald-600 dark:text-emerald-400">
						<MessageCircle class="size-4" />
					</div>
					<span>微信通知</span>
				</Card.Title>
				<Card.Description>回爬暂停、完成、限流、断线时推到微信。不要和其它 iLink 客户端同时在线。</Card.Description>
			</Card.Header>
			<Card.Content class="space-y-3">
				<div class="flex items-center justify-between gap-3">
					<div class="min-w-0">
						<Label for="ilink-notify" class="text-sm font-normal">启用推送</Label>
						<p class="text-xs text-muted-foreground">未绑定前打开也不会发出去</p>
					</div>
					<Switch
						id="ilink-notify"
						checked={!!app.ilink?.enabled}
						onCheckedChange={(value) => void app.setIlinkEnabled(value)}
						disabled={app.busy}
					/>
				</div>
				{#if app.ilink?.qrUrl && (app.ilink.qrState === 'wait' || app.ilink.qrState === 'scanned')}
					<div class="flex flex-col items-center gap-2 py-2">
						<img
							src={app.ilink.qrUrl}
							alt="微信登录二维码"
							class="size-44 rounded-2xl bg-white p-2.5 shadow-sm"
						/>
						<p class="text-xs font-medium text-muted-foreground">{qrLabel(app.ilink.qrState)}</p>
					</div>
				{:else if app.ilink?.qrState === 'expired'}
					<p class="text-xs text-muted-foreground">{qrLabel(app.ilink.qrState)}</p>
				{/if}
				{#if app.ilink?.loggedIn && !app.ilink.bound}
					<p class="text-xs text-muted-foreground">
						已登录。请用该微信给 ClawBot 发一条「绑定」，之后才能推送。
					</p>
				{:else if app.ilink?.bound}
					<p class="text-xs text-muted-foreground">
						已绑定{app.ilink.boundUserHint ? ` ${app.ilink.boundUserHint}` : ''}，可推送暂停 / 完成 /
						异常。
					</p>
				{:else if !app.ilink?.qrUrl}
					<p class="text-xs text-muted-foreground">扫码绑定微信 ClawBot，用同一微信给机器人发一条「绑定」。</p>
				{/if}
				{#if app.ilink?.lastError}
					<p class="text-xs font-medium text-destructive">{app.ilink.lastError}</p>
				{/if}
				<div class="flex flex-wrap gap-2 pt-1">
					<Button
						variant="outline"
						size="sm"
						class="rounded-full px-4 text-xs"
						onclick={() => void app.startIlinkLogin()}
						disabled={app.busy}
					>
						{app.ilink?.loggedIn ? '重新扫码' : '扫码绑定'}
					</Button>
					<Button
						variant="outline"
						size="sm"
						class="rounded-full px-4 text-xs"
						onclick={() => void app.sendIlinkTest()}
						disabled={app.busy || !app.ilink?.bound}
					>
						发送测试
					</Button>
					{#if app.ilink?.loggedIn}
						<Button
							variant="outline"
							size="sm"
							class="rounded-full px-4 text-xs text-destructive hover:bg-destructive/10"
							onclick={() => {
								if (confirm('退出微信绑定？不会影响 Telegram 会话和已下载文件。')) {
									void app.logoutIlink();
								}
							}}
							disabled={app.busy}
						>
							退出登录
						</Button>
					{/if}
				</div>
			</Card.Content>
		</Card.Root>

		<Card.Root class="rounded-3xl border border-border/50 bg-card shadow-xs">
			<Card.Header>
				<Card.Title class="flex items-center gap-2.5">
					<div class="flex size-8 items-center justify-center rounded-2xl bg-amber-500/10 text-amber-600 dark:text-amber-400">
						<ScrollText class="size-4" />
					</div>
					<span>运行日志</span>
				</Card.Title>
				<Card.Description>最近 50 条警告 / 错误，重启后清空</Card.Description>
				<Card.Action>
					<Button
						variant="outline"
						size="sm"
						class="rounded-full px-3 text-xs"
						onclick={() => void loadLogs(true)}
						disabled={logsBusy}
					>
						刷新
					</Button>
				</Card.Action>
			</Card.Header>
			<Card.Content>
				{#if logs.length === 0}
					<p class="py-4 text-center text-xs text-muted-foreground">暂无记录</p>
				{:else}
					<ul class="max-h-72 space-y-2 overflow-y-auto pr-1">
						{#each logs as entry, i (`${i}-${entry.time}`)}
							<li class="space-y-1 rounded-2xl border border-border/40 bg-muted/20 p-2.5">
								<div class="flex items-center gap-2">
									<span class="text-muted-foreground tabular-nums text-[11px]">{entry.time}</span>
									<Badge variant={entry.level === 'error' ? 'destructive' : 'secondary'} class="rounded-full px-1.5 py-0 text-[10px]">
										{entry.level === 'error' ? '错误' : '警告'}
									</Badge>
								</div>
								<p class="font-mono text-[11px] leading-snug break-words">{entry.message}</p>
							</li>
						{/each}
					</ul>
				{/if}
			</Card.Content>
		</Card.Root>

		{#if app.loginStep !== 'authorized'}
			<Card.Root class="rounded-3xl border border-border/50 bg-card shadow-xs">
				<Card.Header>
					<Card.Title>登录</Card.Title>
					<Card.Description>手机号用国际格式。验证码会发到 Telegram 或短信。</Card.Description>
				</Card.Header>
				<Card.Content class="space-y-4">
					<div class="space-y-2">
						<Label for="phone">手机号</Label>
						<div class="flex flex-col gap-2 sm:flex-row">
							<Input
								id="phone"
								bind:value={app.phone}
								placeholder="+86 138 0000 0000"
								autocomplete="tel"
								class="h-9 rounded-full px-3 text-xs"
								disabled={app.busy || app.loginStep === 'needPassword'}
							/>
							<Button
								class="rounded-full px-5 text-xs sm:shrink-0"
								onclick={() => void app.sendCode()}
								disabled={app.busy || !app.phone.trim() || app.loginStep === 'needPassword'}
							>
								{app.busy && app.loginStep === 'idle' ? '发送中…' : '发送验证码'}
							</Button>
						</div>
					</div>

					{#if app.loginStep === 'needCode' || app.loginStep === 'needPassword'}
						<div class="space-y-2">
							<Label for="code">验证码</Label>
							<div class="flex flex-col gap-2 sm:flex-row">
								<Input
									id="code"
									bind:value={app.code}
									placeholder="12345"
									inputmode="numeric"
									autocomplete="one-time-code"
									class="h-9 rounded-full px-3 text-xs"
									disabled={app.busy || app.loginStep === 'needPassword'}
								/>
								<Button
									class="rounded-full px-5 text-xs sm:shrink-0"
									onclick={() => void app.confirmCode()}
									disabled={app.busy || !app.code.trim() || app.loginStep === 'needPassword'}
								>
									{app.busy && app.loginStep === 'needCode' ? '确认中…' : '确认'}
								</Button>
							</div>
						</div>
					{/if}

					{#if app.loginStep === 'needPassword'}
						<div class="space-y-2">
							<Label for="password">两步验证密码</Label>
							{#if app.telegram?.passwordHint}
								<p class="text-xs text-muted-foreground">提示：{app.telegram.passwordHint}</p>
							{/if}
							<div class="flex flex-col gap-2 sm:flex-row">
								<Input
									id="password"
									type="password"
									bind:value={app.password}
									placeholder="账号两步验证密码"
									autocomplete="current-password"
									class="h-9 rounded-full px-3 text-xs"
									disabled={app.busy}
									onkeydown={(event) => app.onPasswordKeydown(event)}
								/>
								<Button
									class="rounded-full px-5 text-xs sm:shrink-0"
									onclick={() => void app.confirmPassword()}
									disabled={app.busy || !app.password.trim()}
								>
									{app.busy ? '登录中…' : '登录'}
								</Button>
							</div>
						</div>
					{/if}
				</Card.Content>
			</Card.Root>
		{/if}

		<Card.Root class="rounded-3xl border border-border/50 bg-card shadow-xs">
			<Card.Header>
				<Card.Title class="flex items-center gap-2.5">
					<div class="flex size-8 items-center justify-center rounded-2xl bg-destructive/10 text-destructive">
						<LogOut class="size-4" />
					</div>
					<span>退出</span>
				</Card.Title>
				<Card.Description>
					{#if isTauri()}
						关窗口会隐藏到托盘，不会退出。退出只走这里或托盘菜单。
					{:else}
						浏览器里不能停服务。请到飞牛应用中心停止纸飞机下载器。消息库和已下载文件会保留。
					{/if}
				</Card.Description>
			</Card.Header>
			{#if isTauri()}
				<Card.Content>
					<Button variant="destructive" class="rounded-full px-5 text-xs" onclick={() => commands.quitApp()}>退出应用</Button>
				</Card.Content>
			{/if}
		</Card.Root>
	</div>
</div>
