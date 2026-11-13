package azemu;

import java.lang.annotation.*;
import org.junit.jupiter.api.extension.*;

/** Starts one emulator for the test class and stops it afterwards. Env entries are "NAME=value". */
@Retention(RetentionPolicy.RUNTIME)
@Target(ElementType.TYPE)
@Inherited
@ExtendWith(EmuTest.Ext.class)
public @interface EmuTest {
    String[] env() default {};

    class Ext implements BeforeAllCallback, AfterAllCallback {
        private static final ExtensionContext.Namespace NS = ExtensionContext.Namespace.create(Ext.class);

        @Override public void beforeAll(ExtensionContext ctx) throws Exception {
            EmuTest a = ctx.getRequiredTestClass().getAnnotation(EmuTest.class);
            java.util.Map<String, String> env = new java.util.HashMap<>();
            if (a != null) for (String e : a.env()) { int i = e.indexOf('='); env.put(e.substring(0, i), e.substring(i + 1)); }
            Emu emu = Emu.start(ctx.getRequiredTestClass().getSimpleName(), env);
            ctx.getStore(NS).put("emu", emu);
        }

        @Override public void afterAll(ExtensionContext ctx) {
            Emu emu = ctx.getStore(NS).remove("emu", Emu.class);
            if (emu != null) emu.close();
        }
    }
}
