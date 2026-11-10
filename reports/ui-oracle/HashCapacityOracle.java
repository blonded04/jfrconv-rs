import java.lang.reflect.Field;
import java.util.HashMap;
public class HashCapacityOracle {
  static int capacity(HashMap<Integer,Integer> m) throws Exception {
    Field f=HashMap.class.getDeclaredField("table");f.setAccessible(true);return ((Object[])f.get(m)).length;
  }
  public static void main(String[] args) throws Exception {
    System.out.println(System.getProperty("java.runtime.version"));
    HashMap<Integer,Integer> head=new HashMap<>(),tail=new HashMap<>();
    for(int i=1;i<=3074;i++){
      head.computeIfAbsent(i,k->k);tail.put(i,i);
      if(i==13||i==14||i==3073||i==3074)
        System.out.println("n="+i+" compute="+capacity(head)+" put="+capacity(tail));
    }
    HashMap<Integer,Integer> edge=new HashMap<>();for(int i=1;i<=3073;i++)edge.computeIfAbsent(i,k->k);
    System.out.println("existing-key resize before="+capacity(edge));edge.computeIfAbsent(1,k->k);
    System.out.println("existing-key resize after="+capacity(edge));
    HashMap<Integer,Integer> kinds=new HashMap<>();kinds.computeIfAbsent((1<<28)|2,k->1);kinds.computeIfAbsent((5<<28)|2,k->5);
    System.out.println("same-name compute kind order="+kinds.values());
    HashMap<Integer,Integer> imported=new HashMap<>();imported.put((1<<28)|2,1);imported.put((5<<28)|2,5);
    System.out.println("same-name put kind order="+imported.values());
  }
}
