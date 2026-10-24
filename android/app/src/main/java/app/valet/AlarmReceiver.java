package app.valet;

import android.app.AlarmManager;
import android.app.Notification;
import android.app.NotificationChannel;
import android.app.NotificationManager;
import android.app.PendingIntent;
import android.content.BroadcastReceiver;
import android.content.Context;
import android.content.Intent;
import android.media.AudioAttributes;
import android.media.RingtoneManager;
import android.os.Bundle;
import android.text.format.DateFormat;

import java.time.Instant;
import java.time.LocalDate;
import java.time.ZoneId;
import java.time.format.DateTimeFormatter;
import java.util.Date;
import java.util.Locale;

/**
 * An alarm going off: a notification that keeps ringing (the alarm sound,
 * through Do Not Disturb's "alarms") until stopped or snoozed, shown full
 * screen over the lock screen when allowed.
 */
public class AlarmReceiver extends BroadcastReceiver {
    static final String RING = "app.valet.RING";
    static final String STOP = "app.valet.STOP";
    static final String SNOOZE = "app.valet.SNOOZE";
    private static final String CHANNEL = "alarms";
    private static final long MINUTE = 60_000L;

    @Override
    public void onReceive(Context c, Intent intent) {
        NotificationManager nm = c.getSystemService(NotificationManager.class);
        Bundle about = about(intent);
        long at = intent.getLongExtra("at", System.currentTimeMillis());
        int id = intent.getIntExtra("id", (int) (at / MINUTE % 1_000_000) + 1);
        String action = intent.getAction() == null ? "" : intent.getAction();
        switch (action) {
            case RING:
                ring(c, nm, id, about, at);
                break;
            case STOP:
                nm.cancel(id);
                break;
            case SNOOZE:
                nm.cancel(id);
                long again = System.currentTimeMillis() + 10 * MINUTE;
                Alarms.set(c, again, Alarms.ring(c, 10_000 + id % 10_000, about, again));
                break;
            default:
                break;
        }
    }

    /** The event an alarm is for, from its intent: title, start (ms, 0 if unknown) or day (all day). */
    static Bundle about(Intent i) {
        Bundle b = new Bundle();
        b.putString("title", i.getStringExtra("title"));
        b.putLong("start", i.getLongExtra("start", 0));
        b.putString("day", i.getStringExtra("day"));
        return b;
    }

    static String title(Bundle about) {
        String t = about.getString("title");
        return t == null || t.isEmpty() ? "Alarm" : t;
    }

    /** The event's time, to show big: its start ("14:30"), "All day", or the alarm's own if unknown. */
    static String clock(Context c, Bundle about, long at) {
        if (about.getString("day") != null) return "All day";
        long start = about.getLong("start");
        return DateFormat.getTimeFormat(c).format(new Date(start > 0 ? start : at));
    }

    /** When the event is, from `now`: "In 12 minutes", "Started 5 minutes ago", with its day unless today. */
    static String when(Bundle about, long now) {
        ZoneId zone = ZoneId.systemDefault();
        LocalDate today = LocalDate.now(zone);
        String day = about.getString("day");
        if (day != null) return day(LocalDate.parse(day), today);
        long start = about.getLong("start");
        if (start == 0) return "";
        long m = Math.round((start - now) / (double) MINUTE);
        String rel = m == 0 ? "Now" : m > 0 ? "In " + span(m) : "Started " + span(-m) + " ago";
        LocalDate d = Instant.ofEpochMilli(start).atZone(zone).toLocalDate();
        return d.equals(today) ? rel : rel + " · " + day(d, today);
    }

    private static String span(long m) {
        if (m < 60) return m + (m == 1 ? " minute" : " minutes");
        long h = m / 60;
        if (h < 24) return h + (h == 1 ? " hour" : " hours") + (m % 60 > 0 ? " " + m % 60 + " min" : "");
        long d = Math.round(m / 1440.0);
        return d + (d == 1 ? " day" : " days");
    }

    private static String day(LocalDate d, LocalDate today) {
        if (d.equals(today)) return "Today";
        if (d.equals(today.plusDays(1))) return "Tomorrow";
        if (d.equals(today.minusDays(1))) return "Yesterday";
        return d.format(DateTimeFormatter.ofPattern("EEE d MMM", Locale.getDefault()));
    }

    static PendingIntent action(Context c, String action, int id, Bundle about, long at) {
        Intent i = new Intent(c, AlarmReceiver.class).setAction(action).putExtras(about).putExtra("id", id).putExtra("at", at);
        return PendingIntent.getBroadcast(c, id * 4 + (STOP.equals(action) ? 1 : 2), i, PendingIntent.FLAG_IMMUTABLE | PendingIntent.FLAG_UPDATE_CURRENT);
    }

    private static void ring(Context c, NotificationManager nm, int id, Bundle about, long at) {
        NotificationChannel ch = new NotificationChannel(CHANNEL, "Alarms", NotificationManager.IMPORTANCE_HIGH);
        ch.setDescription("Alerts of the patterns set to ring");
        AudioAttributes alarm = new AudioAttributes.Builder().setUsage(AudioAttributes.USAGE_ALARM).setContentType(AudioAttributes.CONTENT_TYPE_SONIFICATION).build();
        ch.setSound(RingtoneManager.getDefaultUri(RingtoneManager.TYPE_ALARM), alarm);
        ch.enableVibration(true);
        ch.setVibrationPattern(new long[] {0, 800, 400, 800});
        ch.setBypassDnd(true);
        ch.setLockscreenVisibility(Notification.VISIBILITY_PUBLIC);
        nm.createNotificationChannel(ch);

        Intent screen = new Intent(c, RingActivity.class).putExtras(about).putExtra("id", id).putExtra("at", at)
            .addFlags(Intent.FLAG_ACTIVITY_NEW_TASK | Intent.FLAG_ACTIVITY_NO_USER_ACTION);
        PendingIntent full = PendingIntent.getActivity(c, id, screen, PendingIntent.FLAG_IMMUTABLE | PendingIntent.FLAG_UPDATE_CURRENT);
        String when = when(about, System.currentTimeMillis());
        Notification n = new Notification.Builder(c, CHANNEL)
            .setSmallIcon(R.drawable.ic_alarm)
            .setContentTitle(title(about))
            .setContentText(when.isEmpty() ? clock(c, about, at) : clock(c, about, at) + " · " + when)
            .setCategory(Notification.CATEGORY_ALARM)
            .setVisibility(Notification.VISIBILITY_PUBLIC)
            .setOngoing(true)
            .setFullScreenIntent(full, true)
            .setContentIntent(full)
            .addAction(new Notification.Action.Builder(null, "Stop", action(c, STOP, id, about, at)).build())
            .addAction(new Notification.Action.Builder(null, "Snooze 10 min", action(c, SNOOZE, id, about, at)).build())
            .build();
        n.flags |= Notification.FLAG_INSISTENT;
        nm.notify(id, n);
        // Nobody there: it stops by itself after ten minutes.
        c.getSystemService(AlarmManager.class).set(AlarmManager.RTC_WAKEUP, System.currentTimeMillis() + 10 * MINUTE, action(c, STOP, id, about, at));
    }
}
