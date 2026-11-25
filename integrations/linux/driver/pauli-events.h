/* SPDX-License-Identifier: GPL-2.0-only */
/* One optional board IRQ. Only FID3 goes to HS here; no management queue calls. */
#define EVENT_CLASSES	(PAULI_EVENT_MAIL | PAULI_EVENT_LOG | PAULI_EVENT_STATE)
#define EVENT_REASON_COUNT	4
#define EVENT_DRAIN_LIMIT	8
#define EVENT_STUCK_LIMIT	32

struct pauli_file {
	struct mutex consume;	/* Serializes ARM/READ across dup and fork. */
	u64 binding, seen[EVENT_REASON_COUNT];
	u32 mask;
};

static DEFINE_MUTEX(events_lock);
static DECLARE_WAIT_QUEUE_HEAD(events_wait);
static struct platform_device *events_dev;
static bool events_registered;
static int events_irq, events_error, feature_error;
static u64 events_binding, generations[EVENT_REASON_COUNT];
static unsigned long subscribers[3];
static u32 event_features, supported, armed;
static unsigned int suspect_irqs;
static unsigned long storm_start;

static int event_sbi(unsigned long command, u32 mask, unsigned long *value)
{
	struct sbiret ret = sbi_ecall(PAULI_SBI_EID, 3, command, mask, 0, 0, 0, 0);

	*value = ret.value;
	return hv_errno(ret.error);
}

static int set_event_mask(u32 mask, unsigned long *previous)
{
	int err = event_sbi(1, mask, previous);

	return !err && (*previous & ~((unsigned long)EVENT_CLASSES)) ? -EIO : err;
}

static void event_wake(__poll_t flags)
{
	wake_up_interruptible_poll(&events_wait, flags);
}

/* Terminal until platform remove. IRQF_ONESHOT keeps the line masked while
 * this thread disables it. Never wait for this IRQ with events_lock held.
 */
static void event_failed(int error)
{
	unsigned long previous;

	events_error = error;
	disable_irq_nosync(events_irq);
	set_event_mask(0, &previous);
	armed = 0;
}

static irqreturn_t event_thread(int irq, void *data)
{
	unsigned int n, bit;
	__poll_t wake = 0;
	unsigned long reasons;
	int err;

	mutex_lock(&events_lock);
	if (data != events_dev || !events_irq || events_error)
		goto out;
	for (n = 0; n < EVENT_DRAIN_LIMIT; n++) {
		err = event_sbi(0, 0, &reasons);
		if (err || (reasons & ~((unsigned long)supported | PAULI_EVENT_RESCAN))) {
			event_failed(err ? err : -EIO);
			break;
		}
		if (!reasons)
			break;
		for (bit = 0; bit < EVENT_REASON_COUNT; bit++)
			if (reasons & (1U << bit))
				generations[bit]++;
		wake = EPOLLIN | EPOLLRDNORM;
	}
	if (!events_error) {
		/* One empty claim is normal after a raced TAKE. A continuously held
		 * wire must not consume a CPU even if HS returns a repeating reason.
		 */
		if (n && n < EVENT_DRAIN_LIMIT) {
			suspect_irqs = 0;
		} else {
			unsigned long now = jiffies;

			if (!suspect_irqs || time_after_eq(now, storm_start + HZ)) {
				suspect_irqs = 0;
				storm_start = now;
			}
			if (++suspect_irqs >= EVENT_STUCK_LIMIT)
				event_failed(-EIO);
		}
	}
	if (events_error)
		wake |= EPOLLERR;
out:
	mutex_unlock(&events_lock);
	if (wake)
		event_wake(wake);
	return IRQ_HANDLED;
}

static int hv_open(struct inode *inode, struct file *file)
{
	struct pauli_file *ctx = kzalloc(sizeof(*ctx), GFP_KERNEL);

	if (!ctx)
		return -ENOMEM;
	mutex_init(&ctx->consume);
	file->private_data = ctx;
	return nonseekable_open(inode, file);
}

static int event_status(struct pauli_file *ctx)
{
	if (ctx->binding && (ctx->binding != events_binding || !events_dev))
		return -ENODEV;
	if (events_error)
		return events_error;
	return events_irq ? 0 : -EOPNOTSUPP;
}

/* Change HS only when the union of all subscriptions changes. Commit new
 * references/cursors after the successful call, preserving overlapping bits.
 */
static int change_subscription(struct pauli_file *ctx, u32 mask)
{
	u32 next = 0, added = mask & ~ctx->mask;
	unsigned long previous;
	unsigned int i;
	int err;

	for (i = 0; i < 3; i++) {
		u32 bit = 1U << i;
		if ((mask & bit) || subscribers[i] > !!(ctx->mask & bit))
			next |= bit;
	}
	if (next != armed) {
		err = set_event_mask(next, &previous);
		if (err || previous != armed) {
			event_failed(err ? err : -EIO);
			return events_error;
		}
		armed = next;
	}
	if (!ctx->mask && mask)
		added |= PAULI_EVENT_RESCAN;
	for (i = 0; i < EVENT_REASON_COUNT; i++) {
		u32 bit = 1U << i;
		if (added & bit)
			ctx->seen[i] = generations[i];
		if (i < 3 && ((mask ^ ctx->mask) & bit)) {
			if (mask & bit)
				subscribers[i]++;
			else
				subscribers[i]--;
		}
	}
	ctx->mask = mask;
	ctx->binding = mask ? events_binding : 0;
	return 0;
}

static int hv_release(struct inode *inode, struct file *file)
{
	struct pauli_file *ctx = file->private_data;
	int err = 0;

	mutex_lock(&events_lock);
	if (ctx->mask && !event_status(ctx))
		err = change_subscription(ctx, 0);
	mutex_unlock(&events_lock);
	if (err)
		event_wake(EPOLLERR);
	kfree(ctx);
	return 0;
}

static long hv_event_info(void __user *arg)
{
	struct pauli_event_info info = {};
	int err;

	mutex_lock(&events_lock);
	info.features = event_features;
	if (events_dev)
		info.flags |= PAULI_EVENT_DT;
	if (events_irq && !events_error) {
		info.flags |= PAULI_EVENT_READY;
		info.supported = supported;
	}
	err = events_error;
	mutex_unlock(&events_lock);
	if (err && err != -EOPNOTSUPP)
		return err;
	return copy_to_user(arg, &info, sizeof(info)) ? -EFAULT : 0;
}

static long hv_event_arm(struct file *file, void __user *arg)
{
	struct pauli_file *ctx = file->private_data;
	struct pauli_event_arm req;
	bool failed = false;
	int err;

	if (copy_from_user(&req, arg, sizeof(req)))
		return -EFAULT;
	if (req.reserved || (req.mask & ~EVENT_CLASSES))
		return -EINVAL;
	mutex_lock(&ctx->consume);
	mutex_lock(&events_lock);
	err = event_status(ctx);
	if (!err && (req.mask & ~supported))
		err = -EOPNOTSUPP;
	if (!err) {
		err = change_subscription(ctx, req.mask);
		failed = err != 0;
	}
	mutex_unlock(&events_lock);
	mutex_unlock(&ctx->consume);
	if (failed)
		event_wake(EPOLLERR);
	return err;
}

static u32 pending_events(struct pauli_file *ctx)
{
	unsigned int i;
	u32 pending = 0;

	if (ctx->mask)
		for (i = 0; i < EVENT_REASON_COUNT; i++)
			if (((ctx->mask | PAULI_EVENT_RESCAN) & (1U << i)) &&
			    ctx->seen[i] != generations[i])
				pending |= 1U << i;
	return pending;
}

static __poll_t hv_poll(struct file *file, poll_table *wait)
{
	struct pauli_file *ctx = file->private_data;
	__poll_t mask = 0;
	int err;

	if (!capable(CAP_SYS_ADMIN))
		return EPOLLERR;
	poll_wait(file, &events_wait, wait);
	mutex_lock(&events_lock);
	err = event_status(ctx);
	if (err == -ENODEV)
		mask = EPOLLERR | EPOLLHUP;
	else if (err)
		mask = EPOLLERR;
	else if (pending_events(ctx))
		mask = EPOLLIN | EPOLLRDNORM;
	mutex_unlock(&events_lock);
	return mask;
}

static long hv_event_read(struct file *file, void __user *arg)
{
	struct pauli_file *ctx = file->private_data;
	struct pauli_events out = {};
	u64 sample[EVENT_REASON_COUNT];
	int err;

	mutex_lock(&ctx->consume);
	mutex_lock(&events_lock);
	err = event_status(ctx);
	if (!err) {
		out.reasons = pending_events(ctx);
		memcpy(sample, generations, sizeof(sample));
	}
	mutex_unlock(&events_lock);
	if (!err) {
		if (copy_to_user(arg, &out, sizeof(out))) {
			err = -EFAULT;
		} else {
			/* The IRQ can publish another generation during user copy. */
			mutex_lock(&events_lock);
			memcpy(ctx->seen, sample, sizeof(sample));
			mutex_unlock(&events_lock);
		}
	}
	mutex_unlock(&ctx->consume);
	return err;
}

/* The most recently registered managed action runs before devres frees the
 * threaded IRQ. Its state is detached before an in-flight handler can publish.
 */
static void event_detach(void *data)
{
	unsigned long previous;

	mutex_lock(&events_lock);
	if (events_irq && !events_error)
		set_event_mask(0, &previous);
	events_irq = 0;
	events_dev = NULL;
	events_error = feature_error;
	armed = supported = 0;
	memset(subscribers, 0, sizeof(subscribers));
	mutex_unlock(&events_lock);
	event_wake(EPOLLERR | EPOLLHUP);
}

static int event_probe(struct platform_device *pdev)
{
	unsigned long previous;
	int irq, err;
	u32 classes = EVENT_CLASSES;

	if (!of_device_is_compatible(pdev->dev.of_node, "openion,pauli-control") ||
	    !of_device_is_available(pdev->dev.of_node))
		return -ENODEV;
	irq = platform_get_irq_optional(pdev, 0);
	if (irq == -EPROBE_DEFER)
		return irq;
	mutex_lock(&events_lock);
	if (events_dev) {
		mutex_unlock(&events_lock);
		return -EBUSY;
	}
	events_dev = pdev;
	events_binding++;
	suspect_irqs = 0;
	if (!(event_features & PAULI_EVENT_FEAT))
		goto out;
	if (irq <= 0) {
		/* OF reports -ENXIO when this node has no interrupt resource. */
		if (irq < 0 && irq != -ENXIO)
			events_error = irq;
		goto out;
	}
	err = set_event_mask(0, &previous);
	if (err)
		goto failed;
	/* Both protocol generations advertise FEAT_EVENTS. A1 accepts MAIL only. */
	err = set_event_mask(classes, &previous);
	if (err == -EINVAL) {
		classes = PAULI_EVENT_MAIL;
		err = set_event_mask(classes, &previous);
	}
	if (err || previous != 0) {
		set_event_mask(0, &previous);
		if (!err)
			err = -EIO;
		goto failed;
	}
	err = set_event_mask(0, &previous);
	if (!err && previous != classes)
		err = -EIO;
	if (err)
		goto failed;
	err = devm_request_threaded_irq(&pdev->dev, irq, NULL, event_thread,
					IRQF_ONESHOT, "pauli-control", pdev);
	if (err)
		goto failed;
	events_irq = irq;
	supported = classes;
	goto out;
failed:
	events_error = err;
out:
	mutex_unlock(&events_lock);
	/* Keep the OF binding for accurate capability reporting and legacy ioctls. */
	return devm_add_action_or_reset(&pdev->dev, event_detach, pdev);
}

static const struct of_device_id event_match[] = {
	{ .compatible = "openion,pauli-control" }, {}
};
MODULE_DEVICE_TABLE(of, event_match);
static struct platform_driver event_driver = {
	.probe = event_probe,
	.driver = { .name = "pauli-control", .of_match_table = event_match },
};

static void hv_events_register(struct sbiret features)
{
	int err;

	event_features = features.error ? 0 : (u32)features.value;
	feature_error = hv_errno(features.error);
	events_error = feature_error;
	err = platform_driver_register(&event_driver);
	events_registered = !err;
	if (err)
		events_error = err;
}

static void hv_events_unregister(void)
{
	if (events_registered)
		platform_driver_unregister(&event_driver);
	events_registered = false;
}
