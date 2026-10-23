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

import java.text.DateFormat;
import java.util.Date;

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
        String title = intent.getStringExtra("title");
        long at = intent.getLongExtra("at", System.currentTimeMillis());
        int id = intent.getIntExtra("id", (int) (at / MINUTE % 1_000_000) + 1);
        String action = intent.getAction() == null ? "" : intent.getAction();
        switch (action) {
            case RING:
                ring(c, nm, id, title == null ? "" : title, at);
                break;
            case STOP:
                nm.cancel(id);
                break;
            case SNOOZE:
                nm.cancel(id);
                long again = System.currentTimeMillis() + 10 * MINUTE;
                Alarms.set(c, again, Alarms.ring(c, 10_000 + id % 10_000, title, again));
                break;
            default:
                break;
        }
    }

    static PendingIntent action(Context c, String action, int id, String title, long at) {
        Intent i = new Intent(c, AlarmReceiver.class).setAction(action).putExtra("id", id).putExtra("title", title).putExtra("at", at);
        return PendingIntent.getBroadcast(c, id * 4 + (STOP.equals(action) ? 1 : 2), i, PendingIntent.FLAG_IMMUTABLE | PendingIntent.FLAG_UPDATE_CURRENT);
    }

    private static void ring(Context c, NotificationManager nm, int id, String title, long at) {
        NotificationChannel ch = new NotificationChannel(CHANNEL, "Alarms", NotificationManager.IMPORTANCE_HIGH);
        ch.setDescription("Alerts of the patterns set to ring");
        AudioAttributes alarm = new AudioAttributes.Builder().setUsage(AudioAttributes.USAGE_ALARM).setContentType(AudioAttributes.CONTENT_TYPE_SONIFICATION).build();
        ch.setSound(RingtoneManager.getDefaultUri(RingtoneManager.TYPE_ALARM), alarm);
        ch.enableVibration(true);
        ch.setVibrationPattern(new long[] {0, 800, 400, 800});
        ch.setBypassDnd(true);
        ch.setLockscreenVisibility(Notification.VISIBILITY_PUBLIC);
        nm.createNotificationChannel(ch);

        Intent screen = new Intent(c, RingActivity.class).putExtra("id", id).putExtra("title", title).putExtra("at", at)
            .addFlags(Intent.FLAG_ACTIVITY_NEW_TASK | Intent.FLAG_ACTIVITY_NO_USER_ACTION);
        PendingIntent full = PendingIntent.getActivity(c, id, screen, PendingIntent.FLAG_IMMUTABLE | PendingIntent.FLAG_UPDATE_CURRENT);
        Notification n = new Notification.Builder(c, CHANNEL)
            .setSmallIcon(R.drawable.ic_alarm)
            .setContentTitle(title.isEmpty() ? "Alarm" : title)
            .setContentText(DateFormat.getTimeInstance(DateFormat.SHORT).format(new Date(at)))
            .setCategory(Notification.CATEGORY_ALARM)
            .setVisibility(Notification.VISIBILITY_PUBLIC)
            .setOngoing(true)
            .setFullScreenIntent(full, true)
            .setContentIntent(full)
            .addAction(new Notification.Action.Builder(null, "Stop", action(c, STOP, id, title, at)).build())
            .addAction(new Notification.Action.Builder(null, "Snooze 10 min", action(c, SNOOZE, id, title, at)).build())
            .build();
        n.flags |= Notification.FLAG_INSISTENT;
        nm.notify(id, n);
        // Nobody there: it stops by itself after ten minutes.
        c.getSystemService(AlarmManager.class).set(AlarmManager.RTC_WAKEUP, System.currentTimeMillis() + 10 * MINUTE, action(c, STOP, id, title, at));
    }
}
