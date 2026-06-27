package {
    public final class Listener {
        private var name:String;

        public function Listener(name:String) {
            this.name = name;
        }

        public function handle(event:*):void {
            trace(name);
        }
    }
}
