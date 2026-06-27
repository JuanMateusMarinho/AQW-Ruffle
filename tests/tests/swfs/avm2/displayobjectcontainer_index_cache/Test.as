package {
    import flash.display.Sprite;

    [SWF(width="100", height="100", frameRate="24")]
    public final class Test extends Sprite {
        public function Test() {
            var container:Sprite = new Sprite();
            var children:Array = [];
            addChild(container);

            for (var i:int = 0; i < 12; ++i) {
                var child:Sprite = new Sprite();
                child.name = "c" + i;
                children.push(child);
                container.addChild(child);
            }

            trace(order(container));
            trace(indices(container, children));

            container.swapChildrenAt(2, 9);
            trace(order(container));
            trace(container.getChildIndex(children[2]) + "," + container.getChildIndex(children[9]));

            container.setChildIndex(children[0], 11);
            trace(order(container));

            container.removeChild(children[5]);
            container.addChildAt(children[5], 3);
            trace(order(container));
            trace(indices(container, children));

            container.swapChildren(children[1], children[11]);
            trace(order(container));
        }

        private function order(container:Sprite):String {
            var names:Array = [];
            for (var i:int = 0; i < container.numChildren; ++i) {
                names.push(container.getChildAt(i).name);
            }
            return names.join(",");
        }

        private function indices(container:Sprite, children:Array):String {
            var result:Array = [];
            for each (var child:Sprite in children) {
                result.push(container.getChildIndex(child));
            }
            return result.join(",");
        }
    }
}
