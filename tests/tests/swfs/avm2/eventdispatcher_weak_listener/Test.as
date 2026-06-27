package {
    import flash.display.Sprite;
    import flash.events.Event;
    import flash.events.EventDispatcher;
    import flash.system.System;

    [SWF(width="100", height="100", frameRate="60")]
    public final class Test extends Sprite {
        private var strongDispatcher:EventDispatcher = new EventDispatcher();
        private var weakDispatcher:EventDispatcher = new EventDispatcher();
        private var strongListener:Listener = new Listener("strong");
        private var ticks:uint = 0;

        public function Test() {
            strongDispatcher.addEventListener("test", strongListener.handle, false, 0, false);
            weakDispatcher.addEventListener("test", new Listener("weak").handle, false, 0, true);

            trace("initial strong: " + strongDispatcher.hasEventListener("test"));
            trace("initial weak: " + weakDispatcher.hasEventListener("test"));

            addEventListener(Event.ENTER_FRAME, onEnterFrame);
        }

        private function onEnterFrame(event:Event):void {
            ++ticks;

            if (ticks < 4) {
                allocateGarbage();
                System.gc();
                System.gc();
                return;
            }

            trace("collected strong: " + strongDispatcher.hasEventListener("test"));
            trace("collected weak: " + weakDispatcher.hasEventListener("test"));
            strongDispatcher.dispatchEvent(new Event("test"));
            weakDispatcher.dispatchEvent(new Event("test"));
            removeEventListener(Event.ENTER_FRAME, onEnterFrame);
        }

        private function allocateGarbage():void {
            var garbage:Array = [];
            for (var i:uint = 0; i < 10000; ++i) {
                garbage.push({index: i});
            }
        }
    }
}
