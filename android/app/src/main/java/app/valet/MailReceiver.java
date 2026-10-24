package app.valet;

import android.content.BroadcastReceiver;
import android.content.Context;
import android.content.Intent;

/** A look for new mail (every quarter of an hour), or a notification's "Mark as read". */
public class MailReceiver extends BroadcastReceiver {
    static final String CHECK = "app.valet.MAIL";
    static final String READ = "app.valet.READ";

    @Override
    public void onReceive(Context c, Intent intent) {
        PendingResult done = goAsync();
        Context app = c.getApplicationContext();
        new Thread(() -> {
            try {
                if (READ.equals(intent.getAction())) Mail.markRead(app, intent.getLongExtra("message", 0), intent.getStringExtra("tag"));
                else Mail.check(app);
            } finally {
                done.finish();
            }
        }).start();
    }
}
