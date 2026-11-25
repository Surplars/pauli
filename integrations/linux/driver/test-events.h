/* SPDX-License-Identifier: GPL-2.0-only */
/* The real fops, platform probe and threaded IRQ run with the local kernel shims. */
static_assert(PAULI_EVENT_INFO == 0x8010507cU);
static_assert(PAULI_EVENT_ARM == 0x4008507dU);
static_assert(PAULI_EVENT_READ == 0x8008507eU);

static struct platform_device event_node = { .dev.of_node = "openion,pauli-control", .irq = 12 };

static int event_ioctl(struct file *f, unsigned int cmd, void *arg)
{
	return registered->fops->unlocked_ioctl(f, cmd, (unsigned long)arg);
}
static int subscribe(struct file *f, u32 mask)
{
	struct pauli_event_arm arm = { .mask = mask };

	return event_ioctl(f, PAULI_EVENT_ARM, &arm);
}
static u32 consume(struct file *f)
{
	struct pauli_events out = { ~0U, ~0U };

	assert(!event_ioctl(f, PAULI_EVENT_READ, &out) && !out.reserved);
	return out.reasons;
}
static __poll_t ready(struct file *f)
{
	return registered->fops->poll(f, NULL);
}
static void thread_run(void)
{
	assert(irq_thread && irq_thread(event_node.irq, irq_data) == IRQ_HANDLED);
}
static void publish_mail(void)
{
	hs_reasons = PAULI_EVENT_MAIL;
	thread_run();
}
static struct pauli_event_info capabilities(void)
{
	struct pauli_event_info info;

	assert(!event_ioctl(NULL, PAULI_EVENT_INFO, &info) && !info.reserved);
	return info;
}
static void reset_features(unsigned long feats)
{
	hv_exit();
	mock.reply = (struct sbiret){0, 1ULL << 32 | 8};
	mock.features = (struct sbiret){0, feats};
	assert(!hv_init());
}

static void test_event_cursors(void)
{
	struct file a, b, c;
	struct file *dup = &a;
	const __poll_t readable = EPOLLIN | EPOLLRDNORM;
	struct pauli_event_info info;
	struct pauli_events out;
	unsigned int calls;

	assert(!platform->probe(&event_node));
	info = capabilities();
	assert(info.features == 3 && info.flags == (PAULI_EVENT_DT | PAULI_EVENT_READY));
	assert(info.supported == 7 && hs_mask == 0);
	assert(!registered->fops->open(NULL, &a) && !registered->fops->open(NULL, &b));
	assert(!registered->fops->open(NULL, &c));
	assert(!subscribe(&a, PAULI_EVENT_MAIL) && !subscribe(&b, PAULI_EVENT_MAIL));
	assert(!subscribe(&c, PAULI_EVENT_STATE) && hs_mask == 5);
	assert(!ready(&a) && !ready(&b));
	calls = mock.ecalls;
	assert(subscribe(&a, PAULI_EVENT_RESCAN) == -EINVAL);
	struct pauli_event_arm bad = { .mask = 2, .reserved = 1 };
	assert(event_ioctl(&a, PAULI_EVENT_ARM, &bad) == -EINVAL && mock.ecalls == calls);

	/* Publication racing poll_wait must be visible to the same poll call. */
	on_poll_wait = publish_mail;
	assert(ready(&a) == readable && last_wake == readable);
	calls = mock.ecalls;
	assert(ready(&a) == readable && ready(dup) == readable && ready(&b) == readable);
	assert(!ready(&c) && mock.ecalls == calls);
	mock.write_fault = true;
	assert(event_ioctl(&a, PAULI_EVENT_READ, &out) == -EFAULT);
	mock.write_fault = false;
	assert(ready(&a) == readable && consume(dup) == PAULI_EVENT_MAIL);
	assert(!ready(&a) && consume(&a) == 0 && ready(&b) == readable);

	/* A later generation during copy survives committing the earlier sample. */
	on_copy = publish_mail;
	assert(consume(&b) == PAULI_EVENT_MAIL && ready(&b) == readable);
	assert(consume(&b) == PAULI_EVENT_MAIL && !ready(&b));
	assert(consume(&a) == PAULI_EVENT_MAIL);

	hs_reasons = PAULI_EVENT_STATE | PAULI_EVENT_RESCAN;
	thread_run();
	mock.privileged = false;
	calls = mock.ecalls;
	assert(ready(&a) == EPOLLERR && event_ioctl(&a, PAULI_EVENT_READ, &out) == -EPERM);
	assert(subscribe(&a, 0) == -EPERM && event_ioctl(&a, PAULI_EVENT_INFO, &info) == -EPERM);
	assert(mock.ecalls == calls);
	mock.privileged = true;
	assert(consume(&a) == PAULI_EVENT_RESCAN && consume(&b) == PAULI_EVENT_RESCAN);
	assert(consume(&c) == (PAULI_EVENT_STATE | PAULI_EVENT_RESCAN));

	hs_reasons = PAULI_EVENT_MAIL;
	thread_run();
	assert(!subscribe(&a, PAULI_EVENT_MAIL | PAULI_EVENT_LOG));
	assert(consume(&a) == PAULI_EVENT_MAIL && hs_mask == 7); /* Preserve overlap. */
	hs_reasons = PAULI_EVENT_LOG;
	thread_run();
	assert(consume(&a) == PAULI_EVENT_LOG && consume(&c) == 0);
	assert(!subscribe(&b, 0) && hs_mask == 7 && !ready(&b));
	assert(!registered->fops->release(NULL, &c) && hs_mask == 3);
	unbind_events();
	assert(hs_mask == 0 && frees && (last_wake & EPOLLHUP));
	assert(ready(&a) == (EPOLLERR | EPOLLHUP));
	assert(event_ioctl(&a, PAULI_EVENT_READ, &out) == -ENODEV);
	assert(subscribe(&a, 1) == -ENODEV);
	assert(!capabilities().flags);
	assert(!platform->probe(&event_node));
	assert(ready(&a) == (EPOLLERR | EPOLLHUP)); /* No reuse across rebind. */
	assert(!subscribe(&b, 1) && hs_mask == 1); /* Previously disarmed. */
	assert(!registered->fops->release(NULL, &a) && hs_mask == 1);
	assert(!registered->fops->release(NULL, &b) && hs_mask == 0);
	unbind_events();
}

static void test_stuck(bool repeating)
{
	struct file f;
	struct pauli_events out;
	unsigned int disabled_before = disables;
	unsigned int takes_before;

	assert(!platform->probe(&event_node));
	assert(!registered->fops->open(NULL, &f) && !subscribe(&f, 7));
	for (unsigned int i = 0; i < 2 * EVENT_STUCK_LIMIT; i++) {
		jiffies += HZ + 1;
		thread_run();
	}
	assert(!ready(&f) && disables == disabled_before);
	jiffies += HZ + 1;
	takes_before = takes;
	repeat_reason = repeating;
	hs_reasons = repeating ? 1 : 0;
	for (unsigned int i = 0; i < EVENT_STUCK_LIMIT; i++)
		thread_run();
	assert(takes - takes_before == EVENT_STUCK_LIMIT * (repeating ? EVENT_DRAIN_LIMIT : 1));
	assert(disables == disabled_before + 1 && hs_mask == 0 && ready(&f) == EPOLLERR);
	assert((last_wake & EPOLLERR) && event_ioctl(&f, PAULI_EVENT_READ, &out) == -EIO);
	takes_before = takes;
	thread_run();
	assert(takes == takes_before && disables == disabled_before + 1);
	repeat_reason = false;
	hs_reasons = 0;
	assert(!registered->fops->release(NULL, &f));
	unbind_events();
}

static void test_events(void)
{
	struct pauli_event_info info;
	struct file f;

	reset_features(3);
	assert(capabilities().features == 3 && !capabilities().flags);
	assert(!registered->fops->open(NULL, &f) && subscribe(&f, 1) == -EOPNOTSUPP);
	event_node.dev.of_node = NULL;
	assert(platform->probe(&event_node) == -ENODEV && !irq_thread);
	event_node.dev.of_node = "openion,pauli-control";
	event_node.irq = -EPROBE_DEFER;
	assert(platform->probe(&event_node) == -EPROBE_DEFER && !capabilities().flags);
	event_node.irq = -ENXIO;
	assert(!platform->probe(&event_node));
	assert(capabilities().flags == PAULI_EVENT_DT && !capabilities().supported);
	assert(subscribe(&f, 1) == -EOPNOTSUPP);
	unbind_events();
	event_node.irq = 12;
	reset_features(1);
	assert(!platform->probe(&event_node) && !irq_thread);
	assert(capabilities().flags == PAULI_EVENT_DT && subscribe(&f, 1) == -EOPNOTSUPP);
	unbind_events();
	reset_features(3);
	hs_supported = PAULI_EVENT_MAIL;
	assert(!platform->probe(&event_node));
	assert(capabilities().supported == 1 && subscribe(&f, 2) == -EOPNOTSUPP);
	assert(!subscribe(&f, 1) && !subscribe(&f, 0));
	unbind_events();
	hs_supported = 7;
	request_irq_err = -EBUSY;
	assert(!platform->probe(&event_node) && !irq_thread);
	assert(event_ioctl(&f, PAULI_EVENT_INFO, &info) == -EBUSY);
	unbind_events();
	request_irq_err = 0;
	hs_error = SBI_ERR_DENIED;
	assert(!platform->probe(&event_node) && !irq_thread);
	assert(event_ioctl(&f, PAULI_EVENT_INFO, &info) == -EACCES);
	unbind_events();
	hs_error = 0;
	assert(!registered->fops->release(NULL, &f));
	test_event_cursors();
	test_stuck(false);
	test_stuck(true);
	puts("PASS: Pauli optional control IRQ, A1/A2 probing, independent open/dup cursors, copy/poll races, unbind and bounded storm handling");
}
