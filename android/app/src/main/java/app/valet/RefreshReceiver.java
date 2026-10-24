package app.valet;

import android.content.BroadcastReceiver;
import android.content.Context;
import android.content.Intent;

/** After a restart, an update, a time change, and every few hours: alarms again, and the looks for new mail. */
public class RefreshReceiver extends BroadcastReceiver {
    static final String PERIODIC = "app.valet.REFRESH";

    @Override
    public void onReceive(Context c, Intent intent) {
        PendingResult done = goAsync();
        Context app = c.getApplicationContext();
        new Thread(() -> {
            try {
                Alarms.refresh(app);
                Mail.schedule(app);
            } finally {
                done.finish();
            }
        }).start();
    }
}
