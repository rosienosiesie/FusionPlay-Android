import android.graphics.Bitmap;
import android.media.MediaMetadata;
import android.media.session.PlaybackState;
import android.os.Bundle;
import android.os.IBinder;

import java.lang.reflect.Method;
import java.util.ArrayList;
import java.util.Collections;
import java.util.List;
import java.util.Set;

/**
 * Read-only Android shell probe for inspecting the metadata that MiLink sees.
 *
 * <p>The class is intentionally kept outside the FusionPlay APK. It is built
 * into a temporary dex jar, launched through app_process as the adb shell uid,
 * and never modifies a media session or dispatches a transport command.</p>
 */
public final class MediaSessionProbe {
    private static final int MAX_TEXT_LENGTH = 512;

    private MediaSessionProbe() {
    }

    public static void main(String[] args) throws Exception {
        String packageFilter = args.length == 0 ? "" : args[0];
        String callingPackage = args.length < 2 ? "com.android.shell" : args[1];
        List<?> controllers = querySessionControllers(callingPackage);
        System.out.println("session_count=" + controllers.size());
        for (Object controller : controllers) {
            String packageName = String.valueOf(invokeNoArg(controller, "getPackageName"));
            if (!packageFilter.isEmpty() && !packageFilter.equals(packageName)) {
                continue;
            }
            System.out.println("session.package=" + packageName);
            dumpMetadata((MediaMetadata) invokeNoArg(controller, "getMetadata"));
            dumpPlaybackState((PlaybackState) invokeNoArg(controller, "getPlaybackState"));
        }
    }

    private static List<?> querySessionControllers(String callingPackage) throws Exception {
        Class<?> serviceManagerClass = Class.forName("android.os.ServiceManager");
        Method getService = serviceManagerClass.getDeclaredMethod("getService", String.class);
        IBinder binder = (IBinder) getService.invoke(null, "media_session");
        if (binder == null) {
            throw new IllegalStateException("media_session binder is unavailable");
        }

        Class<?> stubClass = Class.forName("android.media.session.ISessionManager$Stub");
        Method asInterface = stubClass.getDeclaredMethod("asInterface", IBinder.class);
        Object manager = asInterface.invoke(null, binder);
        List<?> emptyResult = Collections.emptyList();
        for (Method method : manager.getClass().getMethods()) {
            if (!"getSessions".equals(method.getName())) {
                continue;
            }
            System.out.println("binder_method=" + method.toGenericString());
            int[] userIds = new int[] {0, -1, 999};
            for (int userId : userIds) {
                Object[] arguments = defaultArguments(
                        method.getParameterTypes(), userId, callingPackage);
                Object result = method.invoke(manager, arguments);
                if (result instanceof List) {
                    List<?> sessions = (List<?>) result;
                    System.out.println("binder_result.user=" + userId + ",count=" + sessions.size());
                    if (!sessions.isEmpty()) {
                        return sessions;
                    }
                    emptyResult = sessions;
                }
            }
        }
        if (emptyResult != null) {
            return emptyResult;
        }
        throw new NoSuchMethodException("ISessionManager.getSessions");
    }

    private static Object[] defaultArguments(
            Class<?>[] parameterTypes, int userId, String callingPackage) {
        Object[] arguments = new Object[parameterTypes.length];
        for (int index = 0; index < parameterTypes.length; index++) {
            Class<?> type = parameterTypes[index];
            if (type == int.class) {
                arguments[index] = Integer.valueOf(userId);
            } else if (type == long.class) {
                arguments[index] = Long.valueOf(0L);
            } else if (type == boolean.class) {
                arguments[index] = Boolean.FALSE;
            } else if (type == String.class) {
                arguments[index] = callingPackage;
            } else {
                arguments[index] = null;
            }
        }
        return arguments;
    }

    private static Object invokeNoArg(Object target, String methodName) throws Exception {
        Method method = target.getClass().getMethod(methodName);
        method.setAccessible(true);
        return method.invoke(target);
    }

    private static void dumpMetadata(MediaMetadata metadata) {
        if (metadata == null) {
            System.out.println("metadata=null");
            return;
        }
        Bundle bundle = metadataBundle(metadata);
        List<String> keys = bundle == null
                ? new ArrayList<>(metadata.keySet())
                : sortedKeys(bundle);
        Collections.sort(keys);
        System.out.println("metadata.key_count=" + keys.size());
        for (String key : keys) {
            Object value = bundle == null ? publicMetadataValue(metadata, key) : bundle.get(key);
            System.out.println("metadata[" + key + "]=" + describe(value));
        }
    }

    private static Bundle metadataBundle(MediaMetadata metadata) {
        try {
            Method getBundle = MediaMetadata.class.getDeclaredMethod("getBundle");
            getBundle.setAccessible(true);
            return (Bundle) getBundle.invoke(metadata);
        } catch (Exception ignored) {
            return null;
        }
    }

    private static Object publicMetadataValue(MediaMetadata metadata, String key) {
        Bitmap bitmap = metadata.getBitmap(key);
        if (bitmap != null) {
            return bitmap;
        }
        CharSequence text = metadata.getText(key);
        if (text != null) {
            return text;
        }
        return Long.valueOf(metadata.getLong(key));
    }

    private static void dumpPlaybackState(PlaybackState state) {
        if (state == null) {
            System.out.println("playback_state=null");
            return;
        }
        System.out.println("playback_state.state=" + state.getState());
        System.out.println("playback_state.position=" + state.getPosition());
        List<PlaybackState.CustomAction> actions = state.getCustomActions();
        System.out.println("playback_state.custom_action_count=" + actions.size());
        for (int index = 0; index < actions.size(); index++) {
            PlaybackState.CustomAction action = actions.get(index);
            String prefix = "custom_action[" + index + "]";
            System.out.println(prefix + ".id=" + safeText(action.getAction()));
            System.out.println(prefix + ".name=" + safeText(action.getName()));
            System.out.println(prefix + ".icon=" + action.getIcon());
            Bundle extras = action.getExtras();
            for (String key : sortedKeys(extras)) {
                System.out.println(prefix + ".extras[" + key + "]=" + describe(extras.get(key)));
            }
        }
    }

    private static List<String> sortedKeys(Bundle bundle) {
        if (bundle == null) {
            return Collections.emptyList();
        }
        Set<String> keySet = bundle.keySet();
        List<String> keys = new ArrayList<>(keySet);
        Collections.sort(keys);
        return keys;
    }

    private static String describe(Object value) {
        if (value == null) {
            return "null";
        }
        if (value instanceof Bitmap) {
            Bitmap bitmap = (Bitmap) value;
            return "Bitmap(" + bitmap.getWidth() + "x" + bitmap.getHeight() + ")";
        }
        return value.getClass().getName() + "(" + safeText(String.valueOf(value)) + ")";
    }

    private static String safeText(CharSequence value) {
        if (value == null) {
            return "null";
        }
        String text = value.toString().replace('\r', ' ').replace('\n', ' ');
        if (text.length() <= MAX_TEXT_LENGTH) {
            return text;
        }
        return text.substring(0, MAX_TEXT_LENGTH) + "…[length=" + text.length() + "]";
    }
}
