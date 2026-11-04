package app.valet;

import android.app.Activity;
import android.content.Intent;
import android.content.res.ColorStateList;
import android.graphics.Color;
import android.graphics.Typeface;
import android.graphics.drawable.GradientDrawable;
import android.graphics.drawable.RippleDrawable;
import android.os.Build;
import android.os.Bundle;
import android.os.Handler;
import android.os.Looper;
import android.text.TextUtils;
import android.util.TypedValue;
import android.view.Gravity;
import android.view.View;
import android.view.WindowManager;
import android.widget.Button;
import android.widget.LinearLayout;
import android.widget.TextView;

import java.time.Instant;
import java.time.LocalDate;
import java.time.ZoneId;
import java.util.Date;

/**
 * An alarm, full screen (over the lock screen): what it's for, a live
 * countdown to its start (counting up once started), Snooze or Stop.
 */
public class RingActivity extends Activity {
    private static final int ACCENT = 0xFF60A5FA;    // blue-400
    private static final int LATE = 0xFFFBBF24;      // amber-400, once started
    private static final int MUTED = 0xFFA8A29E;     // stone-400
    private static final int PRIMARY = 0xFF2563EB;   // valet blue
    private static final int SECONDARY = 0x1FFFFFFF; // white, 12%

    private final Handler tick = new Handler(Looper.getMainLooper());
    private long start;
    private TextView label, count, caption;

    private final Runnable update = new Runnable() {
        @Override
        public void run() {
            long now = System.currentTimeMillis();
            long left = start - now;
            count.setText(countdown(left));
            boolean late = left <= 0;
            caption.setText(late ? "since it started" : "until it starts");
            count.setTextColor(late ? LATE : Color.WHITE);
            label.setTextColor(late ? LATE : ACCENT);
            // Next tick on the second the countdown changes.
            long next = Math.floorMod(left, 1000L);
            tick.postDelayed(this, (next == 0 ? 1000 : next) + 5);
        }
    };

    @Override
    protected void onCreate(Bundle state) {
        super.onCreate(state);
        if (Build.VERSION.SDK_INT < 27) {
            getWindow().addFlags(WindowManager.LayoutParams.FLAG_SHOW_WHEN_LOCKED | WindowManager.LayoutParams.FLAG_TURN_SCREEN_ON);
        }
        getWindow().addFlags(WindowManager.LayoutParams.FLAG_KEEP_SCREEN_ON);
        show(getIntent());
    }

    /** Another alarm while this one shows (one screen for all): show the newer. */
    @Override
    protected void onNewIntent(Intent intent) {
        super.onNewIntent(intent);
        setIntent(intent);
        tick.removeCallbacks(update);
        show(intent);
        if (start > 0) update.run();
    }

    private void show(Intent intent) {
        int id = intent.getIntExtra("id", 0);
        Bundle about = AlarmReceiver.about(intent);
        long at = intent.getLongExtra("at", System.currentTimeMillis());
        String day = about.getString("day");
        start = day == null ? about.getLong("start") : 0;
        LocalDate today = LocalDate.now();

        LinearLayout box = new LinearLayout(this);
        box.setOrientation(LinearLayout.VERTICAL);
        box.setPadding(dp(28), dp(56), dp(28), dp(32));

        // "Today · 14:30", what it is, then the countdown: "12:04", "until it starts".
        LinearLayout middle = new LinearLayout(this);
        middle.setOrientation(LinearLayout.VERTICAL);
        middle.setGravity(Gravity.CENTER);
        label = text("", 15, ACCENT, Typeface.create("sans-serif-medium", Typeface.NORMAL));
        label.setLetterSpacing(0.12f);
        label.setAllCaps(true);
        TextView title = text(AlarmReceiver.title(about), 30, Color.WHITE, Typeface.create("sans-serif", Typeface.NORMAL));
        title.setMaxLines(3);
        title.setEllipsize(TextUtils.TruncateAt.END);
        title.setPadding(0, dp(12), 0, dp(40));
        count = text("", 84, Color.WHITE, Typeface.create("sans-serif-light", Typeface.NORMAL));
        count.setFontFeatureSettings("tnum");
        count.setSingleLine(true);
        caption = text("", 17, MUTED, Typeface.create("sans-serif", Typeface.NORMAL));
        middle.addView(label);
        middle.addView(title);
        middle.addView(count);
        middle.addView(caption);

        if (day != null) {
            label.setText(AlarmReceiver.day(LocalDate.parse(day), today));
            count.setText("All day");
            count.setTextSize(TypedValue.COMPLEX_UNIT_SP, 56);
            caption.setVisibility(View.GONE);
        } else if (start == 0) {
            // Not tied to an event: the alarm's own time.
            label.setText("Alarm");
            count.setText(AlarmReceiver.clock(this, about, at));
            caption.setVisibility(View.GONE);
        } else {
            LocalDate d = Instant.ofEpochMilli(start).atZone(ZoneId.systemDefault()).toLocalDate();
            label.setText(AlarmReceiver.day(d, today) + "  ·  " + android.text.format.DateFormat.getTimeFormat(this).format(new Date(start)));
        }
        box.addView(middle, new LinearLayout.LayoutParams(LinearLayout.LayoutParams.MATCH_PARENT, 0, 1));

        LinearLayout buttons = new LinearLayout(this);
        buttons.setOrientation(LinearLayout.HORIZONTAL);
        Button snooze = button("Snooze 10 min", SECONDARY);
        Button stop = button("Stop", PRIMARY);
        snooze.setOnClickListener(v -> send(AlarmReceiver.SNOOZE, id, about, at));
        stop.setOnClickListener(v -> send(AlarmReceiver.STOP, id, about, at));
        LinearLayout.LayoutParams left = new LinearLayout.LayoutParams(0, dp(64), 1);
        left.rightMargin = dp(6);
        LinearLayout.LayoutParams right = new LinearLayout.LayoutParams(0, dp(64), 1);
        right.leftMargin = dp(6);
        buttons.addView(snooze, left);
        buttons.addView(stop, right);
        box.addView(buttons);
        setContentView(box);
    }

    @Override
    protected void onResume() {
        super.onResume();
        if (start > 0) update.run();
    }

    @Override
    protected void onPause() {
        super.onPause();
        tick.removeCallbacks(update);
    }

    /** Time to `left` ms from now, to the second: "4:07", "1:12:04", "2d 3:12:04"; time since, once past. */
    static String countdown(long left) {
        long s = left > 0 ? (left + 999) / 1000 : -left / 1000;
        long d = s / 86400, h = s / 3600 % 24, m = s / 60 % 60;
        s %= 60;
        if (d > 0) return String.format("%dd %d:%02d:%02d", d, h, m, s);
        if (h > 0) return String.format("%d:%02d:%02d", h, m, s);
        return String.format("%d:%02d", m, s);
    }

    private void send(String action, int id, Bundle about, long at) {
        try {
            AlarmReceiver.action(this, action, id, about, at).send();
        } catch (Exception ignored) {
        }
        finish();
    }

    private TextView text(String s, int sp, int color, Typeface face) {
        TextView t = new TextView(this);
        t.setText(s);
        t.setTextColor(color);
        t.setTextSize(TypedValue.COMPLEX_UNIT_SP, sp);
        t.setTypeface(face);
        t.setGravity(Gravity.CENTER);
        return t;
    }

    private Button button(String s, int color) {
        Button b = new Button(this);
        b.setText(s);
        b.setAllCaps(false);
        b.setTextColor(Color.WHITE);
        b.setTextSize(TypedValue.COMPLEX_UNIT_SP, 18);
        b.setTypeface(Typeface.create("sans-serif-medium", Typeface.NORMAL));
        b.setStateListAnimator(null);
        GradientDrawable pill = new GradientDrawable();
        pill.setColor(color);
        pill.setCornerRadius(dp(32));
        b.setBackground(new RippleDrawable(ColorStateList.valueOf(0x33FFFFFF), pill, null));
        return b;
    }

    private int dp(int v) {
        return Math.round(v * getResources().getDisplayMetrics().density);
    }
}
